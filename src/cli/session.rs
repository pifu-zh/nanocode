//! CLI session state & entry points — Rust port of `src/cli.ts` main() +
//! runAgent. Owns the shared context (ToolContext/QueryParams), drives the
//! agent loop, renders events, runs slash commands, persists messages.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::cli::commands;
use crate::cli::format::{
    cost_divider, draw_input_line, format_thinking, format_tool_error, format_tool_result,
    format_tool_start, input_prompt, render_logo, render_markdown, CostTracker,
};
use crate::cli::permission::{AllowAllGate, InteractiveGate};
use crate::cli::spinner;
use crate::core::agent::{AgentLoop, QueryParams};
use crate::core::api::{get_model_config, ModelClient};
use crate::core::types::{
    AgentEvent, ModelConfig, PermissionDecision, PermissionMode,
};
use crate::context::compaction::ModelCompactor;
use crate::context::git_context::GitContextCache;
use crate::context::session::SessionStore;
use crate::tools::ToolRegistry;

/// Command results flow back to the REPL loop.
pub enum CommandOutcome {
    Message(String),
    Prompt(String), // /init → feed as user input
    Exit,
    Error(String),
}

pub struct SessionState {
    pub api_key: String,
    pub messages: Arc<Mutex<Vec<crate::core::types::Message>>>,
    pub registry: ToolRegistry,
    pub model_config: RwLock<ModelConfig>,
    pub permission_mode: RwLock<PermissionMode>,
    pub cost_tracker: CostTracker,
    pub cwd: PathBuf,
    pub session_id: String,
    pub session_store: SessionStore,
    pub tool_context: crate::core::types::ToolContext,
    pub skills: Arc<RwLock<Vec<crate::skills::SkillDefinition>>>,
    pub sub_agent_slot: Arc<RwLock<Option<Arc<dyn crate::core::types::SubAgentRunner>>>>,
    pub exit_plan_state: Arc<RwLock<Option<PermissionMode>>>,
    pub git_cache: Arc<GitContextCache>,
    pub rules: crate::permissions::RuleSet,
    pub interactive: bool,
    pub persist: bool,
}

impl SessionState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cwd: PathBuf,
        model: &str,
        api_key: &str,
        permission_mode: PermissionMode,
        interactive: bool,
        persist: bool,
    ) -> Self {
        let tool_context = crate::core::types::ToolContext {
            cwd: Arc::new(cwd.clone()),
            file_state: Arc::new(Mutex::new(crate::core::types::FileStateCache::new())),
            file_history: Arc::new(Mutex::new(crate::core::types::FileHistoryState::default())),
            modified_files: Arc::new(Mutex::new(Default::default())),
            session_id: String::new(), // filled below
            cancel: CancellationToken::new(),
            permission_mode: Arc::new(RwLock::new(permission_mode)),
            permission_gate: if interactive {
                Arc::new(InteractiveGate::new())
            } else {
                Arc::new(AllowAllGate)
            },
            sub_agent: Arc::new(Mutex::new(None)),
        };

        let skills = Arc::new(RwLock::new(crate::skills::load_all_skills(&cwd)));
        let sub_agent_slot: Arc<RwLock<Option<Arc<dyn crate::core::types::SubAgentRunner>>>> =
            Arc::new(RwLock::new(None));
        let exit_plan_state: Arc<RwLock<Option<PermissionMode>>> = Arc::new(RwLock::new(None));

        let registry = crate::tools::initialize_all_tools(
            crate::tools::todo::TodoStore::new(),
            skills.clone(),
            sub_agent_slot.clone(),
            exit_plan_state.clone(),
        );

        SessionState {
            api_key: api_key.to_string(),
            messages: Arc::new(Mutex::new(Vec::new())),
            registry,
            model_config: RwLock::new(get_model_config(model)),
            permission_mode: RwLock::new(permission_mode),
            cost_tracker: CostTracker::default(),
            cwd,
            session_id: String::new(),
            session_store: SessionStore::default(),
            tool_context,
            skills,
            sub_agent_slot,
            exit_plan_state,
            git_cache: Arc::new(GitContextCache::default()),
            rules: crate::permissions::RuleSet::default(),
            interactive,
            persist,
        }
    }

    pub fn messages(&self) -> Vec<crate::core::types::Message> {
        self.messages.lock().unwrap().clone()
    }

    pub fn model_config(&self) -> ModelConfig {
        self.model_config.read().unwrap().clone()
    }

    pub fn permission_mode(&self) -> PermissionMode {
        *self.permission_mode.read().unwrap()
    }

    pub fn set_permission_mode(&mut self, mode: PermissionMode) {
        *self.permission_mode.write().unwrap() = mode;
    }

    pub fn set_model(&mut self, model: &str) {
        *self.model_config.write().unwrap() = get_model_config(model);
    }

    pub fn clear_messages(&mut self) {
        self.messages.lock().unwrap().clear();
    }

    pub fn set_messages(&mut self, msgs: Vec<crate::core::types::Message>, session_id: &str) {
        *self.messages.lock().unwrap() = msgs;
        self.session_id = session_id.to_string();
    }

    pub fn cost_tracker(&self) -> &CostTracker {
        &self.cost_tracker
    }

    pub fn tool_count(&self) -> usize {
        self.registry.count()
    }

    pub fn modified_file_count(&self) -> usize {
        self.tool_context.modified_files.lock().unwrap().len()
    }

    pub fn snapshot_count(&self) -> usize {
        self.tool_context.file_history.lock().unwrap().snapshots.len()
    }

    pub fn skills(&self) -> Vec<crate::skills::SkillDefinition> {
        self.skills.read().unwrap().clone()
    }

    pub async fn compact(&mut self) -> Result<(i64, i64), String> {
        let snapshot = self.messages();
        if snapshot.is_empty() {
            return Err("Nothing to compact.".into());
        }
        let compactor = self.compactor();
        let outcome = compactor.compact(snapshot).await?;
        let old = outcome.old_tokens;
        let new = outcome.new_tokens;
        let with_attachments =
            crate::context::compaction::with_post_compact_attachments(outcome.compacted, &self.tool_context.file_state)
                .await;
        *self.messages.lock().unwrap() = with_attachments;
        Ok((old, new))
    }

    fn compactor(&self) -> Arc<dyn crate::core::agent::Compactor> {
        Arc::new(ModelCompactor {
            client: ModelClient::from_env(&self.api_key()),
            api_key: self.api_key(),
            model: self.model_config().model,
            cancel: self.tool_context.cancel.clone(),
        })
    }

    /// Explicit key wins; otherwise env fallbacks (ANTHROPIC_API_KEY →
    /// OPENROUTER_API_KEY, TS createClient parity).
    fn api_key(&self) -> String {
        if !self.api_key.is_empty() {
            return self.api_key.clone();
        }
        std::env::var("ANTHROPIC_API_KEY")
            .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
            .unwrap_or_default()
    }

    fn build_params(&self) -> Arc<QueryParams> {
        Arc::new(QueryParams {
            messages: self.messages.clone(),
            tools: self.registry.all().to_vec(),
            model_caller: Arc::new(ModelClient::from_env(&self.api_key())),
            model_config: self.model_config(),
            system_prompt_blocks: self.system_blocks(),
            max_turns: crate::core::agent::DEFAULT_MAX_TURNS,
            permission_mode: self.permission_mode(),
            enable_thinking: false,
            thinking_budget: None,
            tool_context: self.tool_context.clone(),
            compactor: Some(self.compactor()),
            rules: self.rules.clone(),
        })
    }

    fn system_blocks(&self) -> Vec<crate::core::types::SystemPromptBlock> {
        let claude_md = crate::context::memory::load_claude_md(&self.cwd);
        let git_context = self.git_cache.get(&self.cwd);
        let blocks = crate::prompt::build_system_prompt_blocks(&crate::prompt::BuildSystemPromptParams {
            claude_md: &claude_md,
            git_context: &git_context,
            cwd: &self.cwd.to_string_lossy(),
            model: &self.model_config().model,
        });
        crate::prompt::apply_cache(blocks)
    }

    /// Run the agent loop for the current messages, rendering events and
    /// persisting new messages (cli.ts runAgent + displayEvent).
    pub async fn run_agent(&mut self) {
        let start_len = self.messages.lock().unwrap().len();
        let persist_session = self.persist;
        let cwd = self.cwd.to_string_lossy().to_string();
        let store = SessionStore::default();
        let session_id = self.session_id.clone();
        let cancel = self.tool_context.cancel.clone();

        let mut sp = spinner::spinner(None);
        let current_mode = PermissionMode::Default;

        let stream = AgentLoop { params: self.build_params() }.run();
        tokio::pin!(stream);
        while let Some(event) = stream.next().await {
            match event {
                AgentEvent::AssistantText { text } => {
                    sp.stop();
                    print!("{}", render_markdown(&text));
                }
                AgentEvent::Thinking { text } => {
                    sp.stop();
                    print!("{}", format_thinking(&text));
                }
                AgentEvent::ToolStart { tool_name, input, .. } => {
                    sp.stop();
                    print!("{}", format_tool_start(&tool_name, &summarize_input(&tool_name, &input)));
                }
                AgentEvent::ToolResult { tool_name, result, is_error, .. } => {
                    if is_error {
                        print!("{}", format_tool_error(&result));
                    } else {
                        print!("{}", format_tool_result(&result, false));
                    }
                    let _ = &tool_name;
                    sp = spinner::spinner(None);
                }
                AgentEvent::Compact { old_tokens, new_tokens } => {
                    sp.stop();
                    println!(
                        "\n  {} {}",
                        crate::cli::format::yellow("[compact]"),
                        crate::cli::format::dim(&format!("{old_tokens} → {new_tokens} tokens"))
                    );
                    sp = spinner::spinner(None);
                }
                AgentEvent::Usage { usage } => self.cost_tracker.add(&usage),
                AgentEvent::Error { error } => {
                    sp.stop();
                    eprintln!("\n  {}", crate::cli::format::red(&format!("Error: {error}")));
                }
                AgentEvent::MaxTurnsReached { max_turns } => {
                    sp.stop();
                    println!(
                        "\n  {}",
                        crate::cli::format::yellow(&format!(
                            "Max turns reached ({max_turns}). Use /compact or continue."
                        ))
                    );
                }
                AgentEvent::AssistantMessage { .. } | AgentEvent::TurnComplete { .. } => {
                    sp.stop();
                }
                AgentEvent::ToolUse { .. } => {}
            }
        }
        let _ = cancel;

        // Persist new messages (lazy session init — no empty sessions)
        if persist_session && !session_id.is_empty() {
            for msg in self.messages.lock().unwrap().iter().skip(start_len).cloned().collect::<Vec<_>>() {
                let _ = store.save_message(&session_id, &msg, &cwd);
            }
        }
        let _ = current_mode;
    }

    pub async fn execute_command(&mut self, input: &str) -> CommandOutcome {
        let mut parts = input[1..].splitn(2, ' ');
        let cmd = parts.next().unwrap_or("");
        let args = parts.next().unwrap_or("").trim();
        commands::execute(cmd, args, self).await
    }
}

fn summarize_input(tool_name: &str, input: &serde_json::Value) -> String {
    match tool_name {
        "Bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .chars()
            .take(200)
            .collect(),
        "Read" | "Edit" | "Write" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Glob" => input.get("pattern").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        "Grep" => format!(
            "{} {}",
            input.get("pattern").and_then(|v| v.as_str()).unwrap_or(""),
            input.get("path").and_then(|v| v.as_str()).unwrap_or("")
        ),
        "Agent" => input
            .get("description")
            .or_else(|| input.get("prompt"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .chars()
            .take(100)
            .collect(),
        other => {
            let json = serde_json::to_string(input).unwrap_or_default();
            format!("{other}: {}", json.chars().take(100).collect::<String>())
        }
    }
}

// Silence unused imports used conditionally
#[allow(unused)]
fn _touches() {
    let _ = draw_input_line();
    let _ = input_prompt();
    let _ = render_logo(80);
    let _ = cost_divider("");
    let _ = PermissionDecision::allow();
}

// ---------------------------------------------------------------------------
// One-shot mode
// ---------------------------------------------------------------------------

pub async fn run_one_shot(prompt: &str, cwd: PathBuf, model: &str, skip_permissions: bool) -> i32 {
    let mut state = SessionState::new(
        cwd,
        model,
        "",
        if skip_permissions {
            PermissionMode::BypassPermissions
        } else {
            PermissionMode::Default
        },
        false,
        false,
    );
    state
        .messages
        .lock()
        .unwrap()
        .push(crate::core::types::Message::user_text(prompt));

    let mut sp = spinner::spinner(None);
    let _ = &mut sp;
    state.run_agent().await;
    println!(
        "\n{}",
        cost_divider(&state.cost_tracker.summary(&state.model_config()))
    );
    0
}

// ---------------------------------------------------------------------------
// REPL
// ---------------------------------------------------------------------------

pub async fn run_repl(
    cwd: PathBuf,
    model: &str,
    skip_permissions: bool,
    resume: Option<String>,
) -> i32 {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
        .unwrap_or_default();
    if api_key.is_empty() {
        eprintln!(
            "{}\n{}",
            crate::cli::format::red("Error: ANTHROPIC_API_KEY not set."),
            crate::cli::format::dim("Set it via environment variable or --api-key flag.")
        );
        return 1;
    }

    let mut state = SessionState::new(
        cwd.clone(),
        model,
        &api_key,
        if skip_permissions {
            PermissionMode::BypassPermissions
        } else {
            PermissionMode::Default
        },
        true,
        true,
    );

    // Resume or new session id
    match &resume {
        Some(id) => {
            let store = SessionStore::default();
            let loaded = store.load_session(id);
            if loaded.is_empty() {
                eprintln!(
                    "{}",
                    crate::cli::format::red(&format!("Session {id} not found or empty."))
                );
                return 1;
            }
            state.set_messages(loaded, id);
            println!(
                "{}",
                crate::cli::format::dim(&format!(
                    "  Resumed session {} ({} messages)\n",
                    &id[..8.min(id.len())],
                    state.messages().len()
                ))
            );
        }
        None => {
            state.session_id = SessionStore::new_session_id();
        }
    }

    // Startup banner (BEHAVIOR §2)
    println!();
    println!("{}", render_logo(crate::cli::format::terminal_width()));
    let short_cwd = cwd
        .to_string_lossy()
        .replace(&dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default(), "~");
    println!(
        "{}",
        crate::cli::format::box_draw(&[
            format!("{} {}", crate::cli::format::gold(&crate::cli::format::bold("nanocode")), crate::cli::format::dim(&format!("v{}", crate::VERSION))),
            format!("{} {}", crate::cli::format::dim("Model:"), state.model_config().model),
            format!("{}   {}", crate::cli::format::dim("CWD:"), short_cwd),
        ])
    );
    println!(
        "{}",
        crate::cli::format::dim("  /help for commands · Ctrl+C abort · Ctrl+D exit\n")
    );

    // REPL via rustyline
    let mut rl = rustyline::Editor::<LineHelper, rustyline::history::DefaultHistory>::new()
        .expect("readline editor");
    rl.set_helper(Some(LineHelper));

    let mut input_queue: Vec<String> = Vec::new();

    loop {
        let processing = !input_queue.is_empty();
        if !processing {
            print!("{}", draw_input_line());
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }

        match rl.readline(&input_prompt()) {
            Ok(line) => {
                let _ = rl.add_history_entry(&line);

                let mut full_input = String::new();
                let mut current = line.clone();
                // Backslash continuation
                while current.ends_with('\\') && !current.ends_with("\\\\") {
                    full_input.push_str(&current[..current.len() - 1]);
                    full_input.push('\n');
                    print!("{}", crate::cli::format::dim("… "));
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                    match rl.readline("") {
                        Ok(next) => current = next,
                        Err(_) => break,
                    }
                }
                full_input.push_str(&current);
                let full_input = full_input.trim().to_string();
                if full_input.is_empty() {
                    continue;
                }

                if full_input.starts_with('/') {
                    let rest = full_input.strip_prefix('/').unwrap_or(&full_input);
                    let mut parts = rest.splitn(2, ' ');
                    let cmd = parts.next().unwrap_or("");
                    let args = parts.next().unwrap_or("").trim();
                    match commands::execute(cmd, args, &mut state).await {
                        CommandOutcome::Message(msg) => println!("{msg}"),
                        CommandOutcome::Prompt(p) => {
                            process_input(&mut state, &p, &mut input_queue).await;
                        }
                        CommandOutcome::Exit => {
                            println!("{}", crate::cli::format::dim("Goodbye!"));
                            return 0;
                        }
                        CommandOutcome::Error(e) => {
                            println!("{}", crate::cli::format::red(&format!("Command error: {e}")))
                        }
                    }
                } else {
                    process_input(&mut state, &full_input, &mut input_queue).await;
                }
                println!(
                    "{}",
                    cost_divider(&state.cost_tracker.summary(&state.model_config()))
                );
            }
            Err(rustyline::error::ReadlineError::Interrupted) => {
                // Ctrl+C: abort in-flight work
                state.tool_context.cancel.cancel();
                println!("{}", crate::cli::format::dim("\n[interrupted]"));
            }
            Err(rustyline::error::ReadlineError::Eof) => {
                println!("{}", crate::cli::format::dim("\nGoodbye!"));
                return 0;
            }
            Err(_) => return 0,
        }
    }
}

async fn process_input(state: &mut SessionState, input: &str, _queue: &mut Vec<String>) {
    let user_msg = crate::core::types::Message::user_text(input);
    state.messages.lock().unwrap().push(user_msg);
    state.run_agent().await;
}

/// Minimal rustyline helper: slash-command + file-path completion.
pub struct LineHelper;

impl rustyline::completion::Completer for LineHelper {
    type Candidate = rustyline::completion::Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<rustyline::completion::Pair>)> {
        use rustyline::completion::Pair;
        let (prefix, start) = if line.starts_with('/') && pos <= line.len() {
            let word_end = line[pos..]
                .find(' ')
                .map(|i| pos + i)
                .unwrap_or(line.len());
            (line[pos..word_end].to_string(), 0)
        } else {
            // Path completion on last word
            let start = line[..pos]
                .rfind(' ')
                .map(|i| i + 1)
                .unwrap_or(0);
            (line[start..pos].to_string(), start)
        };

        let mut pairs = Vec::new();

        if line.starts_with('/') {
            for cmd in commands::command_list() {
                if format!("/{}", cmd.name).starts_with(&prefix) {
                    pairs.push(Pair {
                        display: format!("/{}", cmd.name),
                        replacement: format!("/{}", cmd.name),
                    });
                }
            }
        } else if prefix.contains('/') || !prefix.is_empty() {
            // Directory listing completion
            let base = PathBuf::from(&prefix);
            let (dir, partial): (PathBuf, String) = if prefix.ends_with('/') {
                (base, String::new())
            } else {
                match (base.parent(), base.file_name()) {
                    (Some(p), Some(f)) => (p.to_path_buf(), f.to_string_lossy().to_string()),
                    _ => (PathBuf::from("."), prefix.clone()),
                }
            };
            let dir = if dir.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                dir
            };
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with(&partial) && !name.starts_with('.') {
                        let sep = if prefix.ends_with('/') { "" } else { "/" };
                        pairs.push(Pair {
                            display: name.clone(),
                            replacement: format!(
                                "{}{}{}",
                                &prefix[..prefix.rfind('/').map(|i| i + 1).unwrap_or(0)],
                                sep,
                                name
                            ),
                        });
                    }
                }
            }
            return Ok((start, pairs));
        }

        Ok((0, pairs))
    }

}

impl rustyline::highlight::Highlighter for LineHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> std::borrow::Cow<'l, str> {
        // Blue tint for slash commands (BEHAVIOR §2)
        if line.starts_with('/') {
            std::borrow::Cow::Owned(crate::cli::format::blue(line))
        } else {
            std::borrow::Cow::Borrowed(line)
        }
    }
}

impl rustyline::hint::Hinter for LineHelper {
    type Hint = String;
}

impl rustyline::validate::Validator for LineHelper {}

impl rustyline::Helper for LineHelper {}
