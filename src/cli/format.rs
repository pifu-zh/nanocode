//! ANSI formatting — Rust port of `src/utils/format.ts`.
//! Brand colors: soft blue rgb(100,149,237), warm gold rgb(230,190,80).

use crate::core::types::{ModelConfig, TokenUsage};

const ESC: &str = "\x1b[";
const RESET: &str = "\x1b[0m";

pub fn blue(s: &str) -> String {
    format!("{ESC}38;2;100;149;237m{s}{RESET}")
}
pub fn gold(s: &str) -> String {
    format!("{ESC}38;2;230;190;80m{s}{RESET}")
}
pub fn bold(s: &str) -> String {
    format!("{ESC}1m{s}{RESET}")
}
pub fn dim(s: &str) -> String {
    format!("{ESC}2m{s}{RESET}")
}
pub fn red(s: &str) -> String {
    format!("{ESC}31m{s}{RESET}")
}
pub fn yellow(s: &str) -> String {
    format!("{ESC}33m{s}{RESET}")
}
pub fn green(s: &str) -> String {
    format!("{ESC}32m{s}{RESET}")
}
pub fn cyan(s: &str) -> String {
    format!("{ESC}36m{s}{RESET}")
}

// ---------------------------------------------------------------------------
// Tool output blocks (format.ts formatToolStart/formatToolResult)
// ---------------------------------------------------------------------------

const MAX_TOOL_LINES: usize = 20;
const MAX_LINE_CHARS: usize = 200;

pub fn format_tool_start(tool_name: &str, summary: &str) -> String {
    format!(
        "\n  {} {}  {}\n",
        gold("●"),
        bold(&blue(tool_name)),
        dim(summary)
    )
}

pub fn format_tool_result(result: &str, is_error: bool) -> String {
    if result.trim().is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = result.split('\n').collect();
    let truncated = lines.len() > MAX_TOOL_LINES;
    let display: Vec<&str> = if truncated {
        lines[..MAX_TOOL_LINES].to_vec()
    } else {
        lines.clone()
    };

    let body = display
        .iter()
        .map(|line| {
            let mut owned = line.to_string();
            let mut cut = String::new();
            if owned.chars().count() > MAX_LINE_CHARS {
                cut = dim("…");
                owned = owned.chars().take(MAX_LINE_CHARS).collect();
            }
            let colored = if is_error { red(&owned) } else { owned };
            format!("  {} {}{}", dim("┃"), colored, cut)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let footer = if truncated {
        format!(
            "\n  {}",
            dim(&format!(
                "╰─ ({} lines, {} hidden)",
                lines.len(),
                lines.len() - MAX_TOOL_LINES
            ))
        )
    } else {
        String::new()
    };

    format!("{body}{footer}\n")
}

pub fn format_tool_error(result: &str) -> String {
    let short: String = result.chars().take(500).collect();
    short
        .split('\n')
        .map(|l| format!("  {} {}", dim("┃"), red(l)))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

// ---------------------------------------------------------------------------
// Gradient logo (format.ts)
// ---------------------------------------------------------------------------

fn lerp(a: (u8, u8, u8), b: (u8, u8, u8), factor: f64) -> (u8, u8, u8) {
    (
        (a.0 as f64 + (b.0 as f64 - a.0 as f64) * factor).round() as u8,
        (a.1 as f64 + (b.1 as f64 - a.1 as f64) * factor).round() as u8,
        (a.2 as f64 + (b.2 as f64 - a.2 as f64) * factor).round() as u8,
    )
}

const GRADIENT_FROM: (u8, u8, u8) = (100, 149, 237);
const GRADIENT_TO: (u8, u8, u8) = (230, 190, 80);

fn gradient_block(lines: &[&str]) -> String {
    let max_len = lines.iter().map(|l| l.chars().count()).max().unwrap_or(1).max(1);
    lines
        .iter()
        .map(|line| {
            line.chars()
                .enumerate()
                .map(|(col, ch)| {
                    if ch.is_whitespace() {
                        ch.to_string()
                    } else {
                        let factor = col as f64 / (max_len - 1).max(1) as f64;
                        let (r, g, b) = lerp(GRADIENT_FROM, GRADIENT_TO, factor);
                        format!("\x1b[38;2;{r};{g};{b}m{ch}\x1b[0m")
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const LOGO_LARGE: &[&str] = &[
    " ███╗   ██╗ █████╗ ███╗   ██╗ ██████╗  ██████╗ ██████╗ ██████╗ ███████╗",
    " ████╗  ██║██╔══██╗████╗  ██║██╔═══██╗██╔════╝██╔═══██╗██╔══██╗██╔════╝",
    " ██╔██╗ ██║███████║██╔██╗ ██║██║   ██║██║     ██║   ██║██║  ██║█████╗  ",
    " ██║╚██╗██║██╔══██║██║╚██╗██║██║   ██║██║     ██║   ██║██║  ██║██╔══╝  ",
    " ██║ ╚████║██║  ██║██║ ╚████║╚██████╔╝╚██████╗╚██████╔╝██████╔╝███████╗",
    " ╚═╝  ╚═══╝╚═╝  ╚═╝╚═╝  ╚═══╝ ╚═════╝  ╚═════╝ ╚═════╝ ╚═════╝ ╚══════╝",
];

const LOGO_MEDIUM: &[&str] = &[
    " ██╗ ██╗████╗ ██╗ ██╗████╗ ████╗ ████╗ ████╗ █████╗",
    " ███╗██║█╔═█╗███╗██║█╔══█╗█╔══╝█╔═█╗█╔═█╗█╔══╝",
    " █╔██║████║█╔██║█║ █║█║   █║ █║█║ █║███╗ ",
    " █║╚█║█╔═█║█║╚█║█║ █║█║   █║ █║█║ █║█╔═╝ ",
    " █║ ╚║█║ █║█║ ╚║╚███╔╝╚███╗╚███╔╝███╔╝████╗",
    " ╚╝  ╝╚╝ ╚╝╚╝  ╝ ╚══╝  ╚══╝ ╚══╝ ╚══╝ ╚═══╝",
];

pub fn render_logo(terminal_width: usize) -> String {
    if terminal_width >= 72 {
        gradient_block(LOGO_LARGE)
    } else {
        gradient_block(LOGO_MEDIUM)
    }
}

pub fn terminal_width() -> usize {
    // Best-effort without a terminal crate; the spinner/logo degrade gracefully.
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(80)
}

// ---------------------------------------------------------------------------
// Boxes & dividers
// ---------------------------------------------------------------------------

fn strip_ansi(s: &str) -> usize {
    let re = regex::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    re.replace_all(s, "").chars().count()
}

pub fn box_draw(lines: &[String]) -> String {
    let max_len = lines.iter().map(|l| strip_ansi(l)).max().unwrap_or(20);
    let inner = max_len + 2;
    let border = |s: &str| gold(&dim(s));
    let top = border(&format!("╭{}╮", "─".repeat(inner + 2)));
    let bot = border(&format!("╰{}╯", "─".repeat(inner + 2)));
    let body = lines
        .iter()
        .map(|l| {
            let pad = max_len.saturating_sub(strip_ansi(l));
            format!("{}  {}{}  {}", border("│"), l, " ".repeat(pad), border("│"))
        })
        .collect::<Vec<_>>();
    [top]
        .into_iter()
        .chain(body)
        .chain([bot])
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn draw_input_line() -> String {
    format!("{}\n", gold(&"─".repeat(terminal_width())))
}

pub fn input_prompt() -> String {
    format!("{} ", gold("❯"))
}

pub fn cost_divider(summary: &str) -> String {
    dim(&format!("  ─── {summary} ───"))
}

// ---------------------------------------------------------------------------
// Markdown-lite rendering (format.ts renderMarkdown + inlineMarkdown)
// ---------------------------------------------------------------------------

pub fn render_markdown(text: &str) -> String {
    let mut result: Vec<String> = Vec::new();
    let mut in_code_block = false;
    let heading_re = regex::Regex::new(r"^(#{1,3})\s+(.+)").unwrap();
    let ul_re = regex::Regex::new(r"^\s*[-*]\s+").unwrap();
    let ol_re = regex::Regex::new(r"^(\s*)(\d+)\.\s+(.+)").unwrap();

    for line in text.split('\n') {
        if line.trim_start().starts_with("```") {
            if !in_code_block {
                in_code_block = true;
                let lang = line.trim_start()[3..].trim();
                result.push(dim(&format!(
                    "  ┌─ {} {}",
                    if lang.is_empty() { "code" } else { lang },
                    "─".repeat(40usize.saturating_sub(lang.len().max(4)).min(40) + 1)
                )));
            } else {
                in_code_block = false;
                result.push(dim(&format!("  └{}", "─".repeat(44))));
            }
            continue;
        }

        if in_code_block {
            result.push(format!("{}{line}", dim("  │ ")));
            continue;
        }

        if let Some(cap) = heading_re.captures(line) {
            result.push(String::new());
            result.push(bold(&blue(&cap[2])));
            continue;
        }

        if line.starts_with("> ") {
            result.push(format!("{}{}", dim("  │ "), dim(line.strip_prefix("> ").unwrap_or(line))));
            continue;
        }

        if ul_re.is_match(line) {
            let content = ul_re.replace(line, "");
            let indent = line.len() - line.trim_start().len();
            result.push(format!("{}  {} {}", " ".repeat(indent), blue("•"), inline_markdown(&content)));
            continue;
        }

        if let Some(cap) = ol_re.captures(line) {
            result.push(format!(
                "  {} {}",
                blue(&format!("{}.", &cap[2])),
                inline_markdown(&cap[3])
            ));
            continue;
        }

        result.push(inline_markdown(line));
    }

    result.join("\n")
}

fn inline_markdown(text: &str) -> String {
    let bold_re = regex::Regex::new(r"\*\*(.+?)\*\*").unwrap();
    let code_re = regex::Regex::new(r"`([^`]+)`").unwrap();
    let url_re = regex::Regex::new(r"(https?://[^\s)]+)").unwrap();
    let slash_re = regex::Regex::new(r"(^|[^A-Za-z0-9_])(/[a-z][\w-]*)\b").unwrap();

    let mut out = bold_re.replace_all(text, |c: &regex::Captures| bold(&c[1])).to_string();
    out = code_re.replace_all(&out, |c: &regex::Captures| blue(&c[1])).to_string();
    out = url_re.replace_all(&out, |c: &regex::Captures| blue(&c[1])).to_string();
    out = slash_re
        .replace_all(&out, |c: &regex::Captures| format!("{}{}", &c[1], bold(&blue(&c[2]))))
        .to_string();
    out
}

// ---------------------------------------------------------------------------
// Thinking
// ---------------------------------------------------------------------------

pub fn format_thinking(text: &str) -> String {
    text.split('\n')
        .map(|l| dim(&format!("  {l}")))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Cost tracker (utils/cost.ts)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct CostTracker {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_creation_tokens: u64,
    pub turns: u64,
}

impl CostTracker {
    pub fn add(&mut self, usage: &TokenUsage) {
        self.total_input_tokens += usage.input_tokens;
        self.total_output_tokens += usage.output_tokens;
        self.total_cache_read_tokens += usage.cache_read_tokens;
        self.total_cache_creation_tokens += usage.cache_creation_tokens;
        self.turns += 1;
    }

    pub fn total_cost_usd(&self, config: &ModelConfig) -> f64 {
        self.total_input_tokens as f64 * config.price_per_input_token
            + self.total_output_tokens as f64 * config.price_per_output_token
            + self.total_cache_read_tokens as f64 * config.price_per_cache_read
            + self.total_cache_creation_tokens as f64 * config.price_per_cache_write
    }

    pub fn summary(&self, config: &ModelConfig) -> String {
        let mut parts = vec![
            format!("Turn {}", self.turns),
            format!(
                "{} in / {} out",
                format_token_count(self.total_input_tokens),
                format_token_count(self.total_output_tokens)
            ),
        ];
        if self.total_cache_read_tokens > 0 || self.total_cache_creation_tokens > 0 {
            parts.push(format!(
                "cache: {} read / {} write",
                format_token_count(self.total_cache_read_tokens),
                format_token_count(self.total_cache_creation_tokens)
            ));
        }
        parts.push(format_usd(self.total_cost_usd(config)));
        parts.join(" | ")
    }
}

fn format_token_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn format_usd(amount: f64) -> String {
    if amount < 0.001 {
        format!("${amount:.5}")
    } else if amount < 0.01 {
        format!("${amount:.4}")
    } else if amount < 1.0 {
        format!("${amount:.3}")
    } else {
        format!("${amount:.2}")
    }
}

// ---------------------------------------------------------------------------
// Tests — ported from test/utils/{format,cost}.test.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn ansi_helpers() {
        assert_eq!(bold("x"), "\x1b[1mx\x1b[0m");
        assert_eq!(dim("x"), "\x1b[2mx\x1b[0m");
        assert_eq!(red("x"), "\x1b[31mx\x1b[0m");
        assert_eq!(yellow("x"), "\x1b[33mx\x1b[0m");
        assert_eq!(green("x"), "\x1b[32mx\x1b[0m");
        assert_eq!(cyan("x"), "\x1b[36mx\x1b[0m");
        assert!(blue("x").contains("38;2;100;149;237"));
        assert!(gold("x").contains("38;2;230;190;80"));
    }

    #[test]
    fn tool_result_truncation() {
        let out = format_tool_result(&(1..=30).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n"), false);
        let lines = out.lines().count();
        assert!(lines <= 22, "{lines}");
        assert!(out.contains("(30 lines, 10 hidden)"));

        let long_line = "x".repeat(300);
        let out = format_tool_result(&long_line, false);
        assert!(out.contains('…'));
        assert!(strip_ansi_len(&out) < 260);
    }

    fn strip_ansi_len(s: &str) -> usize {
        regex::Regex::new(r"\x1b\[[0-9;]*m")
            .unwrap()
            .replace_all(s, "")
            .chars()
            .count()
    }

    #[test]
    fn empty_result_no_output() {
        assert_eq!(format_tool_result("  ", false), "");
    }

    #[test]
    fn error_result_red_and_capped() {
        let out = format_tool_error(&"y".repeat(600));
        assert!(out.contains("\x1b[31m"));
        assert!(strip_ansi_len(&out) < 600);
    }

    #[test]
    fn logo_picks_size() {
        assert_eq!(render_logo(80).lines().count(), 6);
        assert!(render_logo(80).contains("38;2;")); // gradient applied
        assert_eq!(render_logo(40).lines().count(), 6);
    }

    #[test]
    fn cost_summary_with_and_without_cache() {
        let mut t = CostTracker::default();
        let config = crate::core::api::get_model_config("sonnet");
        assert_eq!(t.summary(&config), "Turn 0 | 0 in / 0 out | $0.00000");

        t.add(&TokenUsage {
            input_tokens: 1234,
            output_tokens: 500,
            cache_read_tokens: 100,
            cache_creation_tokens: 0,
        });
        let s = t.summary(&config);
        assert!(s.starts_with("Turn 1 | 1.2K in / 500 out"));
        assert!(s.contains("cache: 100 read / 0 write"));
        assert!(s.contains('$'));

        let cost = t.total_cost_usd(&config);
        assert!((cost - (1234.0 * 3.0 + 500.0 * 15.0 + 100.0 * 0.3) / 1_000_000.0).abs() < 1e-9);
    }

    #[test]
    fn usd_precision_tiers() {
        // via summary formatting indirectly
        let config = crate::core::api::get_model_config("sonnet");
        let mut t = CostTracker::default();
        t.add(&TokenUsage { input_tokens: 1, output_tokens: 0, cache_read_tokens: 0, cache_creation_tokens: 0 });
        assert!(t.summary(&config).contains("$0.00000"));
    }

    #[test]
    fn markdown_rendering() {
        let out = render_markdown("# Heading\ntext with `code` and **bold**\n- item\n> quote");
        assert!(out.contains("\x1b[1m")); // heading bold
        let plain: String = regex::Regex::new(r"\x1b\[[0-9;]*m")
            .unwrap()
            .replace_all(&out, "")
            .to_string();
        assert!(plain.contains("• item"), "{plain}");
        assert!(plain.contains("│ quote")); // blockquote drops the "> " prefix (TS parity)
    }

    #[test]
    fn code_block_fences() {
        let out = render_markdown("```rust\nfn main() {}\n```");
        assert!(out.contains("┌─ rust"));
        assert!(out.contains("└"));
        assert!(out.contains("fn main() {}"));
    }
}
