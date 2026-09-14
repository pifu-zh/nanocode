//! Git context — Rust port of `src/context/git-context.ts`.
//! Branch / main branch / porcelain status (≤100 lines) / last 10 commits,
//! cached per cwd with a 5-minute TTL.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_STATUS_LINES: usize = 100;
const MAX_LOG_ENTRIES: u32 = 10;
#[allow(dead_code)] // TS used spawn timeout; std::Command lacks it — commands are trusted-fast
const GIT_TIMEOUT_MS: u64 = 5_000;
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

pub struct GitContextCache {
    cache: Mutex<HashMap<String, (String, Instant)>>,
}

impl Default for GitContextCache {
    fn default() -> Self {
        GitContextCache { cache: Mutex::new(HashMap::new()) }
    }
}

impl GitContextCache {
    pub fn clear(&self) {
        self.cache.lock().unwrap().clear();
    }

    fn git(&self, args: &[&str], cwd: &Path) -> Option<String> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn is_git_repo(&self, cwd: &Path) -> bool {
        self.git(&["rev-parse", "--is-inside-work-tree"], cwd).as_deref() == Some("true")
    }

    /// Gather git context for the system prompt (git-context.ts getGitContext).
    pub fn get(&self, cwd: &Path) -> String {
        let key = cwd.to_string_lossy().to_string();
        if let Some((value, ts)) = self.cache.lock().unwrap().get(&key) {
            if ts.elapsed() < CACHE_TTL {
                return value.clone();
            }
        }

        let result = self.compute(cwd);
        self.cache
            .lock()
            .unwrap()
            .insert(key, (result.clone(), Instant::now()));
        result
    }

    fn compute(&self, cwd: &Path) -> String {
        if !self.is_git_repo(cwd) {
            return "Not a git repository.".to_string();
        }

        let branch = self.git(&["rev-parse", "--abbrev-ref", "HEAD"], cwd);
        let main_branch = ["main", "master"]
            .iter()
            .find(|b| {
                self.git(&["rev-parse", "--verify", "--quiet", b], cwd)
                    .is_some()
            })
            .copied();
        let status = self.git(&["status", "--porcelain"], cwd);
        let log = self.git(&["log", "--oneline", &format!("-{MAX_LOG_ENTRIES}")], cwd);

        let mut sections: Vec<String> = Vec::new();

        if let Some(branch) = branch {
            sections.push(format!("Current branch: {branch}"));
        }
        if let Some(main) = main_branch {
            sections.push(format!("Main branch: {main}"));
        }
        match status {
            Some(s) if s.is_empty() => sections.push("Status: Clean working tree".into()),
            Some(s) => {
                let lines: Vec<&str> = s.split('\n').collect();
                if lines.len() > MAX_STATUS_LINES {
                    let truncated = lines[..MAX_STATUS_LINES].join("\n");
                    sections.push(format!(
                        "Status:\n{truncated}\n... ({} more files)",
                        lines.len() - MAX_STATUS_LINES
                    ));
                } else {
                    sections.push(format!("Status:\n{s}"));
                }
            }
            None => {}
        }
        if let Some(log) = log {
            sections.push(format!("Recent commits:\n{log}"));
        }

        sections.join("\n\n")
    }

    /// Short one-line status for prompt bars (getGitStatusShort).
    pub fn short(&self, cwd: &Path) -> String {
        if !self.is_git_repo(cwd) {
            return String::new();
        }
        let Some(branch) = self.git(&["rev-parse", "--abbrev-ref", "HEAD"], cwd) else {
            return String::new();
        };
        let changed = self
            .git(&["status", "--porcelain"], cwd)
            .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0);
        if changed == 0 {
            format!("{branch} (clean)")
        } else {
            format!("{branch} ({changed} changed)")
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — ported from test/context/git-context.test.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn git_init(dir: &Path) {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
    }

    fn git_commit(dir: &Path, msg: &str) {
        std::fs::write(dir.join("f.txt"), format!("content {msg}\n")).unwrap();
        for args in [
            vec!["add", "."],
            vec!["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-m", msg],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(dir)
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .unwrap();
        }
    }

    #[test]
    fn non_git_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cache = GitContextCache::default();
        assert_eq!(cache.get(dir.path()), "Not a git repository.");
        assert_eq!(cache.short(dir.path()), "");
    }

    #[test]
    fn git_repo_branch_status_log() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        git_commit(dir.path(), "first commit");
        let cache = GitContextCache::default();

        let ctx = cache.get(dir.path());
        assert!(ctx.contains("Current branch:"), "{ctx}");
        assert!(ctx.contains("Recent commits:\n"));
        assert!(ctx.contains("first commit"));
        assert!(ctx.contains("Status: Clean working tree"));
        assert_eq!(cache.short(dir.path()), format!("{} (clean)", branch_of(&ctx)));
    }

    fn branch_of(ctx: &str) -> String {
        ctx.lines()
            .find(|l| l.starts_with("Current branch: "))
            .unwrap()
            .trim_start_matches("Current branch: ")
            .to_string()
    }

    #[test]
    fn dirty_tree_shows_changes() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        git_commit(dir.path(), "initial");
        std::fs::write(dir.path().join("new.txt"), "untracked").unwrap();
        let cache = GitContextCache::default();

        let ctx = cache.get(dir.path());
        assert!(ctx.contains("new.txt"), "{ctx}");
        assert!(cache.short(dir.path()).contains("1 changed"));
    }

    #[test]
    fn caching_until_cleared() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        git_commit(dir.path(), "c1");
        let cache = GitContextCache::default();

        let first = cache.get(dir.path());
        git_commit(dir.path(), "c2");
        let second = cache.get(dir.path());
        assert_eq!(first, second, "cached within TTL");

        cache.clear();
        let third = cache.get(dir.path());
        assert_ne!(third, second, "fresh after clear");
        assert!(third.contains("c2"));
    }
}
