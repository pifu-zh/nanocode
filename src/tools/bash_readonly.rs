//! Bash read-only command validator — Rust port of `src/tools/bash-readonly.ts`.
//!
//! THE CRITICAL PERFORMANCE/SAFETY FILE. Decides whether a shell command is
//! safe to run without permission (and concurrently). Allowlist-based:
//! every command part must pass; dangerous constructs reject outright.
//!
//! The TS version used lookbehind regexes; the Rust `regex` crate does not
//! support those, so output-redirection detection is hand-written with
//! identical semantics (see `has_dangerous_redirect`).

use regex::Regex;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Safe command allowlist
// ---------------------------------------------------------------------------

const SAFE_COMMANDS: &[&str] = &[
    // File content viewing
    "cat", "head", "tail", "less", "more",
    // Text processing (read-only)
    "wc", "sort", "uniq", "diff", "comm",
    // File finding
    "find", "ls", "tree",
    // Shell builtins (safe ones)
    "pwd", "echo", "printf",
    // Search tools
    "grep", "egrep", "fgrep", "rg", "ag", "ack",
    // Text transformation (read-only pipeline tools)
    "awk", "sed", "tr", "cut", "paste", "col", "column", "fold", "fmt",
    "expand", "unexpand", "tee",
    // Version control (read operations)
    "git",
    // Version checks
    "node", "python", "python3", "ruby", "perl",
    "cargo", "go", "rustc", "java", "javac", "gcc", "g++", "clang",
    // System info
    "which", "type", "file", "stat", "du", "df",
    "env", "printenv", "date", "uname", "whoami", "id", "hostname",
    // Test/conditionals
    "test", "[", "true", "false",
    // Package list commands
    "npm", "yarn", "pnpm", "pip", "pip3", "gem", "bundle",
    // Path utilities
    "realpath", "dirname", "basename", "readlink",
    // Hash / binary inspection
    "md5sum", "sha256sum", "sha1sum", "shasum", "xxd", "od", "strings", "nm",
    "hexdump",
    // JSON processing
    "jq",
    // Misc safe
    "xargs",
];

fn safe_commands() -> &'static std::collections::HashSet<&'static str> {
    static SET: OnceLock<std::collections::HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| SAFE_COMMANDS.iter().copied().collect())
}

// ---------------------------------------------------------------------------
// Git / package-manager safe subcommands
// ---------------------------------------------------------------------------

const GIT_SAFE_SUBCOMMANDS: &[&str] = &[
    "log", "diff", "show", "status", "branch", "remote", "tag", "rev-parse",
    "rev-list", "describe", "shortlog", "blame", "ls-files", "ls-tree",
    "ls-remote", "cat-file", "name-rev", "config", "for-each-ref",
    "count-objects", "stash",
];

const GIT_STASH_SAFE: &[&str] = &["list", "show"];

const NPM_SAFE_SUBCOMMANDS: &[&str] = &[
    "list", "ls", "view", "info", "show", "search", "outdated", "explain",
    "why", "fund", "audit", "doctor", "config",
];

const YARN_SAFE_SUBCOMMANDS: &[&str] = &["list", "info", "why", "outdated", "config"];

const PIP_SAFE_SUBCOMMANDS: &[&str] = &["list", "show", "freeze", "check"];

// ---------------------------------------------------------------------------
// Dangerous patterns
// ---------------------------------------------------------------------------

/// Precompiled regexes without lookbehind (all safe for `regex` crate).
fn dangerous_regexes() -> &'static Vec<Regex> {
    static RE: OnceLock<Vec<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        vec![
            // Command substitution
            Regex::new(r"\$\(").unwrap(),
            Regex::new(r"`[^`]*`").unwrap(),
            // Process substitution
            Regex::new(r"<\(").unwrap(),
            Regex::new(r">\(").unwrap(),
            // Function definitions
            Regex::new(r"\bfunction\s+\w+").unwrap(),
            Regex::new(r"\w+\s*\(\)\s*\{").unwrap(),
            // Dangerous builtins
            Regex::new(r"\beval\b").unwrap(),
            Regex::new(r"\bexec\b").unwrap(),
            Regex::new(r"\bsource\b").unwrap(),
            Regex::new(r"\b\.\s+\/").unwrap(),
        ]
    })
}

/// Hand-written replacement for the TS lookbehind patterns:
/// `(?<![12<])>(?!&\d|\/dev\/null)` and `/>>(?!\/dev\/null)`.
/// A `>` is dangerous when the previous char is not `1`/`2`/`<` and what
/// follows is neither `&<digit>` nor `/dev/null` (no leading-space tolerance,
/// matching the TS lookahead exactly).
fn has_dangerous_redirect(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    for i in 0..chars.len() {
        if chars[i] != '>' {
            continue;
        }
        let prev_ok = i == 0 || !matches!(chars[i - 1], '1' | '2' | '<');
        if !prev_ok {
            continue;
        }
        let rest: String = chars[i + 1..].iter().collect();
        let first_two = rest.chars().take(2).collect::<String>();
        let amp_digit = first_two.len() == 2
            && first_two.starts_with('&')
            && first_two.chars().nth(1).is_some_and(|c| c.is_ascii_digit());
        if amp_digit {
            continue;
        }
        if rest.starts_with("/dev/null") {
            continue;
        }
        return true;
    }
    false
}

fn has_dangerous_patterns(cmd: &str) -> bool {
    if has_dangerous_redirect(cmd) {
        return true;
    }
    dangerous_regexes().iter().any(|re| re.is_match(cmd))
}

// ---------------------------------------------------------------------------
// Safe environment variable prefixes
// ---------------------------------------------------------------------------

const SAFE_ENV_VARS: &[&str] = &[
    "GOARCH", "GOOS", "GOPATH", "GOROOT", "GOBIN", "GOFLAGS",
    "NODE_ENV", "NODE_OPTIONS", "NODE_PATH",
    "PYTHONPATH", "PYTHONDONTWRITEBYTECODE", "PYTHONUNBUFFERED",
    "RUST_BACKTRACE", "RUST_LOG", "CARGO_HOME",
    "PATH", "HOME", "USER", "SHELL", "TERM", "LANG", "LC_ALL",
    "TZ", "EDITOR", "VISUAL", "PAGER",
    "NO_COLOR", "FORCE_COLOR", "CLICOLOR",
    "GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL", "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "CI", "GITHUB_ACTIONS", "GITLAB_CI",
    "DEBUG", "VERBOSE", "QUIET",
    "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY",
    "FZF_DEFAULT_COMMAND", "FZF_DEFAULT_OPTS",
    "COLUMNS", "LINES",
    "TMPDIR", "TEMP", "TMP",
];

// ---------------------------------------------------------------------------
// Command parsing
// ---------------------------------------------------------------------------

/// Split a compound command on &&, ||, ;, | respecting quotes.
pub fn parse_command_parts(command: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];

        // Escape sequences
        if ch == '\\' && !in_single && i + 1 < chars.len() {
            current.push(ch);
            current.push(chars[i + 1]);
            i += 2;
            continue;
        }

        // Quotes
        if ch == '\'' && !in_double {
            in_single = !in_single;
            current.push(ch);
            i += 1;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            current.push(ch);
            i += 1;
            continue;
        }
        if in_single || in_double {
            current.push(ch);
            i += 1;
            continue;
        }

        // Operators: && || ; |
        if ch == '&' && i + 1 < chars.len() && chars[i + 1] == '&' {
            if !current.trim().is_empty() {
                parts.push(current.trim().to_string());
            }
            current.clear();
            i += 2;
            continue;
        }
        if ch == '|' && i + 1 < chars.len() && chars[i + 1] == '|' {
            if !current.trim().is_empty() {
                parts.push(current.trim().to_string());
            }
            current.clear();
            i += 2;
            continue;
        }
        if ch == ';' || ch == '|' {
            if !current.trim().is_empty() {
                parts.push(current.trim().to_string());
            }
            current.clear();
            i += 1;
            continue;
        }

        current.push(ch);
        i += 1;
    }

    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    parts
}

/// Whitespace tokenizer respecting quotes (bash-readonly.ts tokenize).
fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && !in_single && i + 1 < chars.len() {
            current.push(ch);
            current.push(chars[i + 1]);
            i += 1;
            i += 1;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue_holder(&mut current, ch);
            i += 1;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            current.push(ch);
            i += 1;
            continue;
        }
        if !in_single && !in_double && ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            i += 1;
            continue;
        }
        current.push(ch);
        i += 1;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

// helper to keep tokenize concise; pushes the quote char
fn continue_holder(current: &mut String, ch: char) {
    current.push(ch);
}

struct Extracted {
    env_vars: Vec<String>,
    command: String,
    args: Vec<String>,
}

/// Extract command + args, skipping leading VAR=value assignments.
fn extract_command_and_args(part: &str) -> Extracted {
    let tokens = tokenize(part);
    let mut env_vars = Vec::new();
    let mut command_idx = 0;
    let env_re = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*=").unwrap();

    for (i, token) in tokens.iter().enumerate() {
        if env_re.is_match(token) {
            env_vars.push(token.clone());
            command_idx = i + 1;
        } else {
            break;
        }
    }

    let command = tokens.get(command_idx).cloned().unwrap_or_default();
    let args = tokens.get(command_idx + 1..).map(|s| s.to_vec()).unwrap_or_default();
    Extracted { env_vars, command, args }
}

// ---------------------------------------------------------------------------
// Per-command flag validation (bash-readonly.ts validateFlags)
// ---------------------------------------------------------------------------

pub fn validate_flags(command: &str, args: &[String]) -> bool {
    match command {
        "sed" => {
            // -i / --in-place / -i.bak are dangerous
            !args.iter().any(|a| a == "-i" || a == "--in-place" || a.starts_with("-i"))
        }

        "git" => {
            let subcommand = args.iter().find(|a| !a.starts_with('-'));
            let Some(sub) = subcommand else { return true }; // bare git is safe
            if sub == "stash" {
                // next non-flag token after 'stash'
                let pos = args.iter().position(|a| a == "stash").unwrap_or(0);
                let stash_sub = args[pos + 1..].iter().find(|a| !a.starts_with('-'));
                return stash_sub.is_none_or(|s| GIT_STASH_SAFE.contains(&s.as_str()));
            }
            GIT_SAFE_SUBCOMMANDS.contains(&sub.as_str())
        }

        "npm" | "npx" => {
            // TS: bare npm safe, bare npx not; npx with any subcommand unsafe.
            let sub = args.iter().find(|a| !a.starts_with('-'));
            match sub {
                None => command == "npm",
                Some(_) if command == "npx" => false,
                Some(s) => NPM_SAFE_SUBCOMMANDS.contains(&s.as_str()),
            }
        }

        "yarn" => {
            let sub = args.iter().find(|a| !a.starts_with('-'));
            sub.is_none_or(|s| YARN_SAFE_SUBCOMMANDS.contains(&s.as_str()))
        }

        "pnpm" => {
            let sub = args.iter().find(|a| !a.starts_with('-'));
            sub.is_none_or(|s| NPM_SAFE_SUBCOMMANDS.contains(&s.as_str()))
        }

        "pip" | "pip3" => {
            let sub = args.iter().find(|a| !a.starts_with('-'));
            sub.is_none_or(|s| PIP_SAFE_SUBCOMMANDS.contains(&s.as_str()))
        }

        "cargo" => {
            let sub = args.iter().find(|a| !a.starts_with('-'));
            sub.is_none_or(|s| s == "--version")
        }

        "go" => {
            let sub = args.iter().find(|a| !a.starts_with('-'));
            sub.is_none_or(|s| s == "version" || s == "env" || s == "list")
        }

        "rustc" => args.iter().any(|a| a == "--version" || a == "-V"),

        "node" | "python" | "python3" | "ruby" | "perl" => {
            if args.is_empty() {
                return false;
            }
            args.iter().all(|a| a == "--version" || a == "-v" || a == "-V")
        }

        "jq" => !args.iter().any(|a| a == "-f" || a == "--from-file"),

        "tee" => {
            let non_flags: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
            non_flags.iter().all(|a| a.as_str() == "/dev/null")
        }

        "xargs" => {
            let sub = args.iter().find(|a| !a.starts_with('-'));
            match sub {
                None => false,
                Some(s) => safe_commands().contains(s.as_str()),
            }
        }

        "bundle" => {
            let sub = args.iter().find(|a| !a.starts_with('-'));
            sub.is_none_or(|s| {
                s == "list" || s == "show" || s == "info" || s == "outdated"
            })
        }

        "gem" => {
            let sub = args.iter().find(|a| !a.starts_with('-'));
            sub.is_none_or(|s| {
                s == "list" || s == "search" || s == "info" || s == "environment"
            })
        }

        "find" => !args
            .iter()
            .any(|a| a == "-exec" || a == "-execdir" || a == "-delete" || a == "-ok"),

        "awk" => {
            let program = args
                .iter()
                .find(|a| !a.starts_with('-') && a.as_str() != "-F");
            match program {
                None => true,
                Some(p) => {
                    let sys_re = Regex::new(r"\bsystem\s*\(").unwrap();
                    let redirect_re = Regex::new(r">[^&]").unwrap();
                    !sys_re.is_match(p) && !redirect_re.is_match(p)
                }
            }
        }

        _ => true,
    }
}

fn are_env_vars_safe(env_vars: &[String]) -> bool {
    env_vars.iter().all(|assignment| {
        assignment
            .split_once('=')
            .map(|(name, _)| SAFE_ENV_VARS.contains(&name))
            .unwrap_or(false)
    })
}

/// Check a single command part (no operators) for read-only safety.
pub fn check_safe_command(part: &str) -> bool {
    let trimmed = part.trim();
    if trimmed.is_empty() {
        return true;
    }

    let Extracted { env_vars, command, args } = extract_command_and_args(trimmed);

    if !env_vars.is_empty() && !are_env_vars_safe(&env_vars) {
        return false;
    }

    // Bare env-var assignments are safe
    if command.is_empty() {
        return true;
    }

    // Strip path prefix (/usr/bin/grep → grep)
    let base_command = command.rsplit('/').next().unwrap_or(&command);

    if !safe_commands().contains(base_command) {
        return false;
    }

    validate_flags(base_command, &args)
}

// ---------------------------------------------------------------------------
// Main entry point (bash-readonly.ts isReadOnlyCommand)
// ---------------------------------------------------------------------------

/// Determine whether a shell command is read-only — true only if ALL parts
/// are safe and no dangerous construct is present.
pub fn is_read_only_command(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return true;
    }

    // Dangerous patterns first (fast rejection), with the /dev/null escape:
    // a match that involves only >/dev/null redirections is stripped and the
    // whole set re-checked (TS logic replicated).
    let mut matched_danger = false;
    for re in dangerous_regexes() {
        if re.is_match(trimmed) {
            matched_danger = true;
            break;
        }
    }
    if !matched_danger && has_dangerous_redirect(trimmed) {
        matched_danger = true;
    }

    if matched_danger {
        let dev_null_re = Regex::new(r">\s*/dev/null").unwrap();
        let dev_null_append_re = Regex::new(r">>\s*/dev/null").unwrap();
        let amp1_re = Regex::new(r"2>&1").unwrap();
        if dev_null_re.is_match(trimmed) || dev_null_append_re.is_match(trimmed) {
            let without = amp1_re.replace_all(trimmed, "");
            let without = dev_null_append_re.replace_all(&without, "");
            let without = dev_null_re.replace_all(&without, "");
            if has_dangerous_patterns(&without) {
                return false;
            }
            // Continue with the cleaned command below (TS re-parses trimmed;
            // parsing parts of the original also passes because the remaining
            // text is what matters — we parse the stripped version, which is
            // strictly more permissive and matches TS's end behavior).
            return parse_parts_all_safe(&without);
        }
        return false;
    }

    parse_parts_all_safe(trimmed)
}

fn parse_parts_all_safe(command: &str) -> bool {
    let parts = parse_command_parts(command);
    if parts.is_empty() {
        return true;
    }
    parts.iter().all(|p| check_safe_command(p))
}

// ---------------------------------------------------------------------------
// Tests — ported from test/tools/bash-readonly.test.ts (58 cases)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_commands_pass() {
        for cmd in ["ls", "cat file.txt", "pwd", "echo hello", "git status"] {
            assert!(is_read_only_command(cmd), "{cmd}");
        }
    }

    #[test]
    fn unsafe_commands_rejected() {
        for cmd in [
            "rm file.txt", "mkdir dir", "touch x", "mv a b", "cp a b",
            "chmod +x f", "curl http://x", "wget http://x", "bash script.sh",
            "sh -c 'x'", "python script.py", "dd if=/dev/zero of=x",
            "kill -9 1", "reboot", "sudo ls",
        ] {
            assert!(!is_read_only_command(cmd), "{cmd}");
        }
    }

    #[test]
    fn piped_commands_all_parts_must_be_safe() {
        assert!(is_read_only_command("cat file | grep foo | wc -l"));
        assert!(!is_read_only_command("cat file | rm -rf /"));
        assert!(is_read_only_command("ls | head -5"));
    }

    #[test]
    fn chained_commands() {
        assert!(is_read_only_command("ls && pwd; echo done"));
        assert!(!is_read_only_command("ls && rm x"));
        assert!(!is_read_only_command("ls; touch y"));
        assert!(is_read_only_command("ls || echo fail"));
        assert!(!is_read_only_command("rm x || echo fail"));
    }

    #[test]
    fn output_redirection() {
        assert!(!is_read_only_command("echo hi > file.txt"));
        assert!(!is_read_only_command("echo hi >> file.txt"));
        assert!(is_read_only_command("echo hi > /dev/null"));
        assert!(is_read_only_command("echo hi >> /dev/null"));
        assert!(is_read_only_command("ls 2>/dev/null"));
        assert!(is_read_only_command("ls 2>&1"));
        // redirection to /dev/null but another danger present
        assert!(!is_read_only_command("rm x > /dev/null"));
    }

    #[test]
    fn command_substitution_rejected() {
        assert!(!is_read_only_command("echo $(rm -rf /)"));
        assert!(!is_read_only_command("echo `rm -rf /`"));
        assert!(!is_read_only_command("cat $(ls)"));
        // Even safe-looking substitution is rejected (fail closed)
        assert!(!is_read_only_command("echo $(pwd)"));
    }

    #[test]
    fn process_substitution_rejected() {
        assert!(!is_read_only_command("diff <(ls) <(ls -la)"));
        assert!(!is_read_only_command("cat >(/dev/null)"));
    }

    #[test]
    fn env_vars() {
        assert!(is_read_only_command("NODE_ENV=production node --version"));
        assert!(is_read_only_command("PATH=/usr/bin ls"));
        assert!(!is_read_only_command("LD_PRELOAD=/evil ls"));
        // safe env vars with unsafe command still blocked
        assert!(!is_read_only_command("NODE_ENV=x rm -rf /"));
    }

    #[test]
    fn quoted_strings() {
        assert!(is_read_only_command("grep 'a && b' file.txt"));
        assert!(is_read_only_command("grep \"a | b\" file.txt"));
        // operators inside quotes must not split
        assert!(is_read_only_command("echo 'ls; rm -rf /'"));
        // escaped characters
        assert!(is_read_only_command("grep foo\\ bar file.txt"));
    }

    #[test]
    fn edge_cases() {
        assert!(is_read_only_command(""));
        assert!(is_read_only_command("   "));
        assert!(is_read_only_command("/usr/bin/grep foo f.txt")); // path prefix ok
        assert!(!is_read_only_command("function evil() { rm; }"));
        assert!(!is_read_only_command("f() { rm; }"));
        assert!(!is_read_only_command("eval 'ls'"));
        assert!(!is_read_only_command("exec ls"));
        assert!(!is_read_only_command("source ~/.bashrc"));
        assert!(!is_read_only_command(". ./script.sh"));
    }

    #[test]
    fn git_subcommands() {
        assert!(is_read_only_command("git"));
        assert!(is_read_only_command("git log"));
        assert!(is_read_only_command("git diff HEAD~1"));
        assert!(is_read_only_command("git stash list"));
        assert!(is_read_only_command("git stash show"));
        assert!(!is_read_only_command("git push"));
        assert!(!is_read_only_command("git commit -m x"));
        assert!(!is_read_only_command("git stash pop"));
        assert!(!is_read_only_command("git checkout -b x"));
    }

    #[test]
    fn npm_yarn_pip_subcommands() {
        assert!(is_read_only_command("npm"));
        assert!(is_read_only_command("npm list"));
        assert!(is_read_only_command("npm view pkg"));
        assert!(!is_read_only_command("npm install"));
        assert!(!is_read_only_command("npx"));
        assert!(!is_read_only_command("npx anything"));
        assert!(is_read_only_command("yarn info pkg"));
        assert!(!is_read_only_command("yarn add pkg"));
        assert!(is_read_only_command("pip list"));
        assert!(is_read_only_command("pip3 freeze"));
        assert!(!is_read_only_command("pip install x"));
        assert!(is_read_only_command("bundle list"));
        assert!(!is_read_only_command("bundle install"));
        assert!(is_read_only_command("gem list"));
        assert!(!is_read_only_command("gem install x"));
    }

    #[test]
    fn interpreters_version_only() {
        assert!(!is_read_only_command("node"));
        assert!(is_read_only_command("node --version"));
        assert!(is_read_only_command("python3 -V"));
        assert!(!is_read_only_command("node script.js"));
        assert!(!is_read_only_command("python script.py"));
    }

    #[test]
    fn rust_toolchain() {
        assert!(is_read_only_command("cargo --version"));
        assert!(!is_read_only_command("cargo build"));
        assert!(!is_read_only_command("rustc main.rs"));
        assert!(is_read_only_command("rustc --version"));
        assert!(is_read_only_command("go version"));
        assert!(is_read_only_command("go env"));
        assert!(!is_read_only_command("go build"));
    }

    #[test]
    fn special_flag_rules() {
        assert!(is_read_only_command("sed s/a/b/ file"));
        assert!(!is_read_only_command("sed -i s/a/b/ file"));
        assert!(!is_read_only_command("sed -i.bak s/a/b/ file"));
        assert!(is_read_only_command("echo x | tee /dev/null"));
        assert!(!is_read_only_command("tee out.txt"));
        assert!(is_read_only_command("xargs grep foo")); // safe subcommand
        assert!(!is_read_only_command("xargs rm"));
        assert!(is_read_only_command("jq . file.json"));
        assert!(!is_read_only_command("jq -f prog.jq"));
        assert!(is_read_only_command("find . -name x"));
        assert!(!is_read_only_command("find . -name x -delete"));
        assert!(!is_read_only_command("find . -exec rm {} \\;"));
        assert!(is_read_only_command("awk '{print $1}' f"));
        assert!(!is_read_only_command("awk '{system(\"rm\")}' f"));
    }

    #[test]
    fn parse_parts_operators() {
        assert_eq!(parse_command_parts("a | b"), vec!["a", "b"]);
        assert_eq!(parse_command_parts("a && b"), vec!["a", "b"]);
        assert_eq!(parse_command_parts("a || b"), vec!["a", "b"]);
        assert_eq!(parse_command_parts("a; b"), vec!["a", "b"]);
        assert_eq!(parse_command_parts("'a | b'"), vec!["'a | b'"]);
        assert_eq!(parse_command_parts("\"a && b\""), vec!["\"a && b\""]);
        let mixed = parse_command_parts("a | b && c; d || e");
        assert_eq!(mixed, vec!["a", "b", "c", "d", "e"]);
        assert!(parse_command_parts("").is_empty());
        assert!(parse_command_parts("   ").is_empty());
    }

    #[test]
    fn check_safe_parts() {
        assert!(check_safe_command(""));
        assert!(check_safe_command("   "));
        assert!(check_safe_command("ls -la"));
        assert!(!check_safe_command("rm -rf"));
        assert!(check_safe_command("/bin/ls")); // path prefix stripped
        assert!(!check_safe_command("unknown_cmd"));
    }

    #[test]
    fn validate_flag_cases() {
        let v = |cmd: &str, args: &[&str]| {
            validate_flags(cmd, &args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        assert!(!v("sed", &["-i", "s/a/b/"]));
        assert!(!v("sed", &["-i.bak", "s/a/b/"]));
        assert!(v("sed", &["s/a/b/", "f"]));
        assert!(!v("git", &["push"]));
        assert!(v("git", &["log"]));
        assert!(v("git", &["stash", "list"]));
        assert!(!v("git", &["stash", "pop"]));
        assert!(v("npm", &["list"]));
        assert!(!v("npm", &["install"]));
        assert!(!v("npx", &["anything"]));
        assert!(!v("node", &[]));
        assert!(v("node", &["--version"]));
        assert!(v("jq", &[".", "f"]));
        assert!(!v("jq", &["-f", "p"]));
        assert!(v("find", &[".", "-name", "x"]));
        assert!(!v("find", &["-exec", "rm"]));
        assert!(v("tee", &["/dev/null"]));
        assert!(!v("tee", &["out"]));
        assert!(v("pip", &["list"]));
        assert!(v("yarn", &["info"]));
        assert!(v("unknown", &["anything"])); // default true
    }
}
