//! Memory loader — Rust port of `src/context/memory.ts`.
//! NANOCODE.md / CLAUDE.md hierarchical loading with @include support.

use std::path::{Path, PathBuf};

const MAX_INCLUDE_DEPTH: usize = 5;

/// Config dirs — .nanocode/ takes priority over .claude/ (memory.ts).
const CONFIG_DIRS: &[&str] = &[".nanocode", ".claude"];

/// Files checked at each directory level, in order (memory.ts MEMORY_FILES).
const MEMORY_FILES: &[&str] = &[
    "NANOCODE.md",
    "CLAUDE.md",
    ".nanocode/NANOCODE.md",
    ".nanocode/CLAUDE.md",
    ".claude/NANOCODE.md",
    ".claude/CLAUDE.md",
    "NANOCODE.local.md",
    "CLAUDE.local.md",
];

const RULES_DIRS: &[&str] = &[".nanocode/rules", ".claude/rules"];

struct MemoryFragment {
    source: String,
    content: String,
}

fn try_read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().filter(|c| !c.is_empty())
}

fn is_directory(path: &Path) -> bool {
    path.is_dir()
}

fn list_md_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().map(|e| e == "md").unwrap_or(false))
        .collect();
    out.sort();
    out
}

/// Expand `@include path/to/file.md` recursively (max depth 5).
fn process_includes(content: &str, base_dir: &Path, depth: usize) -> String {
    if depth >= MAX_INCLUDE_DEPTH {
        return content.to_string();
    }
    let mut out = String::new();
    for line in content.split('\n') {
        if let Some(rest) = line.strip_prefix("@include ") {
            let include_path = rest.trim();
            let full = base_dir.join(include_path);
            match try_read(&full) {
                Some(nested) => {
                    out.push_str(&process_includes(
                        &nested,
                        full.parent().unwrap_or(base_dir),
                        depth + 1,
                    ));
                }
                None => {
                    out.push_str(&format!("<!-- @include failed: {include_path} not found -->"));
                }
            }
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    // strip the trailing newline added above for each line
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

fn collect_from_directory(dir: &Path) -> Vec<MemoryFragment> {
    let mut fragments = Vec::new();

    for file in MEMORY_FILES {
        let file_path = dir.join(file);
        if let Some(content) = try_read(&file_path) {
            let processed = process_includes(&content, file_path.parent().unwrap_or(dir), 0);
            if !processed.trim().is_empty() {
                fragments.push(MemoryFragment {
                    source: file_path.to_string_lossy().to_string(),
                    content: processed.trim().to_string(),
                });
            }
        }
    }

    for rules_rel in RULES_DIRS {
        let rules_dir = dir.join(rules_rel);
        if is_directory(&rules_dir) {
            for rule_file in list_md_files(&rules_dir) {
                if let Some(content) = try_read(&rule_file) {
                    let processed = process_includes(&content, rule_file.parent().unwrap_or(&rules_dir), 0);
                    if !processed.trim().is_empty() {
                        fragments.push(MemoryFragment {
                            source: rule_file.to_string_lossy().to_string(),
                            content: processed.trim().to_string(),
                        });
                    }
                }
            }
        }
    }

    fragments
}

/// Walk from cwd upward to the filesystem root (deduped).
fn walk_upward(start: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut current = start.to_path_buf();
    loop {
        out.push(current.clone());
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    out
}

/// Load and merge all NANOCODE.md / CLAUDE.md files: walk from cwd to the
/// filesystem root, then user-level config. Merged with `# Source:` labels.
pub fn load_claude_md(cwd: &Path) -> String {
    let mut fragments: Vec<MemoryFragment> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for dir in walk_upward(cwd) {
        for fragment in collect_from_directory(&dir) {
            if seen.insert(fragment.source.clone()) {
                fragments.push(fragment);
            }
        }
    }

    // User-level: ~/.nanocode/{NANOCODE,CLAUDE}.md → ~/.claude/...
    if let Some(home) = dirs::home_dir() {
        for config_dir in CONFIG_DIRS {
            for md in ["NANOCODE.md", "CLAUDE.md"] {
                let path = home.join(config_dir).join(md);
                let source = path.to_string_lossy().to_string();
                if seen.contains(&source) {
                    continue;
                }
                if let Some(content) = try_read(&path) {
                    let processed = process_includes(&content, path.parent().unwrap_or(&home), 0);
                    if !processed.trim().is_empty() {
                        seen.insert(source.clone());
                        fragments.push(MemoryFragment {
                            source,
                            content: processed.trim().to_string(),
                        });
                    }
                }
            }
        }
    }

    if fragments.is_empty() {
        return String::new();
    }

    let cwd_str = cwd.to_string_lossy();
    fragments
        .into_iter()
        .map(|f| {
            let rel = f
                .source
                .strip_prefix(&format!("{cwd_str}/"))
                .unwrap_or(&f.source);
            format!("# Source: {rel}\n\n{}", f.content)
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

pub fn has_claude_md(cwd: &Path) -> bool {
    for file in MEMORY_FILES {
        if let Some(c) = try_read(&cwd.join(file)) {
            if !c.trim().is_empty() {
                return true;
            }
        }
    }
    for rules in RULES_DIRS {
        let dir = cwd.join(rules);
        if is_directory(&dir) && !list_md_files(&dir).is_empty() {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Tests — ported from test/context/memory.test.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn empty_when_no_files() {
        let dir = setup();
        assert_eq!(load_claude_md(dir.path()), "");
        assert!(!has_claude_md(dir.path()));
    }

    #[test]
    fn finds_files_in_priority_order() {
        let dir = setup();
        std::fs::write(dir.path().join("CLAUDE.md"), "from claude md").unwrap();
        std::fs::create_dir(dir.path().join(".nanocode")).unwrap();
        std::fs::write(dir.path().join(".nanocode/CLAUDE.md"), "from nanocode dir").unwrap();

        let out = load_claude_md(dir.path());
        assert!(out.contains("from claude md"));
        assert!(out.contains("from nanocode dir"));
        // NANOCODE.md/CLAUDE.md checked before .nanocode/CLAUDE.md
        let claude_pos = out.find("from claude md").unwrap();
        let dir_pos = out.find("from nanocode dir").unwrap();
        assert!(claude_pos < dir_pos);
    }

    #[test]
    fn local_md_found() {
        let dir = setup();
        std::fs::write(dir.path().join("CLAUDE.local.md"), "local instructions").unwrap();
        assert!(load_claude_md(dir.path()).contains("local instructions"));
    }

    #[test]
    fn walks_upward_and_dedupes() {
        let parent = setup();
        std::fs::write(parent.path().join("CLAUDE.md"), "parent level").unwrap();
        let child = parent.path().join("sub");
        std::fs::create_dir(&child).unwrap();

        let out = load_claude_md(&child);
        assert_eq!(out.matches("parent level").count(), 1); // deduped
        // The file lives above cwd → TS keeps the absolute path as source
        assert!(out.contains("CLAUDE.md"));
        assert!(out.contains(&parent.path().join("CLAUDE.md").to_string_lossy().to_string()));
    }

    #[test]
    fn rules_directory_sorted() {
        let dir = setup();
        std::fs::create_dir_all(dir.path().join(".claude/rules")).unwrap();
        std::fs::write(dir.path().join(".claude/rules/b.md"), "rule b").unwrap();
        std::fs::write(dir.path().join(".claude/rules/a.md"), "rule a").unwrap();

        let out = load_claude_md(dir.path());
        let a_pos = out.find("rule a").unwrap();
        let b_pos = out.find("rule b").unwrap();
        assert!(a_pos < b_pos, "alphabetical order");
    }

    #[test]
    fn include_directives() {
        let dir = setup();
        std::fs::write(dir.path().join("extra.md"), "included content").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "main\n@include extra.md").unwrap();

        let out = load_claude_md(dir.path());
        assert!(out.contains("included content"));
        assert!(!out.contains("@include extra.md"));
    }

    #[test]
    fn include_missing_replaced_with_warning() {
        let dir = setup();
        std::fs::write(dir.path().join("CLAUDE.md"), "main\n@include missing.md").unwrap();
        let out = load_claude_md(dir.path());
        assert!(out.contains("<!-- @include failed: missing.md not found -->"));
    }

    #[test]
    fn nested_includes_and_depth_limit() {
        let dir = setup();
        std::fs::write(dir.path().join("c.md"), "leaf-c").unwrap();
        std::fs::write(dir.path().join("b.md"), "leaf-b\n@include c.md").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "top\n@include b.md").unwrap();
        let out = load_claude_md(dir.path());
        assert!(out.contains("leaf-b") && out.contains("leaf-c"));
    }

    #[test]
    fn empty_files_skipped() {
        let dir = setup();
        std::fs::write(dir.path().join("CLAUDE.md"), "   \n").unwrap();
        assert_eq!(load_claude_md(dir.path()), "");
        assert!(!has_claude_md(dir.path()));
    }

    #[test]
    fn has_claude_md_variants() {
        let dir = setup();
        assert!(!has_claude_md(dir.path()));
        std::fs::write(dir.path().join("NANOCODE.md"), "x").unwrap();
        assert!(has_claude_md(dir.path()));
    }
}
