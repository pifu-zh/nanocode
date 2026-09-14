//! nanocode CLI entry — Rust port of `src/cli.ts` parseArgs + main().

use clap::Parser;
use std::path::PathBuf;

use nanocode::cli::session::{run_one_shot, run_repl};

#[derive(Parser, Debug)]
#[command(
    name = "nanocode",
    about = "nanocode — Lightweight AI coding agent for the terminal",
    version = nanocode::VERSION,
    disable_help_flag = true,
    disable_version_flag = true
)]
struct CliArgs {
    /// Run single prompt and exit
    #[arg(short = 'p', long = "prompt")]
    prompt: Option<String>,

    /// Model name (default: sonnet)
    #[arg(short = 'm', long = "model")]
    model: Option<String>,

    /// API provider: anthropic (default) or openai
    #[arg(long = "provider")]
    provider: Option<String>,

    /// API key (or set ANTHROPIC_API_KEY)
    #[arg(long = "api-key")]
    api_key: Option<String>,

    /// Max agent turns (default: 200)
    #[arg(long = "max-turns")]
    max_turns: Option<u32>,

    /// default|plan|acceptEdits|bypassPermissions
    #[arg(long = "permission-mode")]
    permission_mode: Option<String>,

    /// Bypass all permission checks
    #[arg(long = "dangerously-skip-permissions")]
    dangerously_skip_permissions: bool,

    /// Resume a previous session
    #[arg(long = "resume")]
    resume: Option<String>,

    /// Enable extended thinking
    #[arg(long = "thinking")]
    thinking: bool,

    /// Show this help
    #[arg(long = "help", short = 'h')]
    help: bool,

    /// Show version
    #[arg(long = "version")]
    version: bool,

    /// Positional prompt (one-shot)
    prompt_positional: Option<String>,
}

impl CliArgs {
    fn print_help(&self) {
        println!(
            "{} — Lightweight Claude Code clone

{}
  nanocode [options]           Start interactive REPL
  nanocode -p \"prompt\"         One-shot mode

{}
  -p, --prompt <text>          Run single prompt and exit
  -m, --model <model>          Model name (default: sonnet)
      --provider <name>        anthropic (default) or openai
      --api-key <key>          API key (provider env var if omitted)
      --max-turns <n>          Max agent turns (default: 200)
      --permission-mode <mode> default|plan|acceptEdits|bypassPermissions
      --dangerously-skip-permissions  Bypass all permission checks
      --thinking               Enable extended thinking
      --resume <session-id>    Resume a previous session
  -h, --help                   Show this help
      --version                Show version

{}
{}",
            nanocode::cli::format::bold("nanocode"),
            nanocode::cli::format::bold("Usage:"),
            nanocode::cli::format::bold("Options:"),
            nanocode::cli::format::bold("Slash Commands:"),
            nanocode::cli::commands::command_list()
                .iter()
                .filter(|c| c.name != "quit")
                .map(|c| format!("  /{:<12} {}", c.name, c.description))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

#[tokio::main]
async fn main() {
    let args = CliArgs::parse();

    if args.version {
        println!("nanocode {}", nanocode::VERSION);
        std::process::exit(0);
    }
    if args.help {
        args.print_help();
        std::process::exit(0);
    }

    // Provider: --provider > NANOCODE_PROVIDER > default anthropic
    let provider_name = args
        .provider
        .clone()
        .or_else(|| std::env::var("NANOCODE_PROVIDER").ok())
        .unwrap_or_else(|| "anthropic".into());
    let Some(provider) = nanocode::core::api::ModelProvider::parse(&provider_name) else {
        eprintln!(
            "{}",
            nanocode::cli::format::red(&format!(
                "Error: unknown provider \"{provider_name}\" (expected anthropic or openai)"
            ))
        );
        std::process::exit(1);
    };

    // API key precedence: --api-key > provider env var
    //   anthropic: ANTHROPIC_API_KEY > OPENROUTER_API_KEY
    //   openai:    OPENAI_API_KEY
    if let Some(key) = &args.api_key {
        std::env::set_var("ANTHROPIC_API_KEY", key);
        std::env::set_var("OPENAI_API_KEY", key);
    }
    let has_key = match provider {
        nanocode::core::api::ModelProvider::Anthropic => std::env::var("ANTHROPIC_API_KEY")
            .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
            .map(|k| !k.is_empty())
            .unwrap_or(false),
        nanocode::core::api::ModelProvider::OpenAI => {
            std::env::var("OPENAI_API_KEY").map(|k| !k.is_empty()).unwrap_or(false)
        }
    };
    if !has_key {
        eprintln!(
            "{}\n{}",
            nanocode::cli::format::red("Error: API key not set."),
            nanocode::cli::format::dim(
                "Set ANTHROPIC_API_KEY (or OPENAI_API_KEY for --provider openai), or use --api-key."
            )
        );
        std::process::exit(1);
    }

    let default_model = match provider {
        nanocode::core::api::ModelProvider::Anthropic => "sonnet".to_string(),
        nanocode::core::api::ModelProvider::OpenAI => "gpt-4o-mini".to_string(),
    };
    let model = args.model.clone().unwrap_or(default_model);
    let prompt = args.prompt.clone().or(args.prompt_positional);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let exit_code = if let Some(prompt) = prompt {
        run_one_shot(&prompt, cwd, &model, provider, args.dangerously_skip_permissions).await
    } else {
        run_repl(
            cwd,
            &model,
            provider,
            args.dangerously_skip_permissions,
            args.resume.clone(),
        )
        .await
    };
    let _ = args.max_turns; // accepted for CLI parity; loop default is 200
    std::process::exit(exit_code);
}
