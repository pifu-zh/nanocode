//! File utilities — Rust port of `src/files/utils.ts` plus the path helpers
//! the TS version got for free from `node:path`.

use std::path::{Component, Path, PathBuf};

/// Absolute canonical form of a path: resolve against cwd, remove `.` and
/// `..` segments lexically. Unlike `canonicalize`, works for nonexistent
/// paths (matching TS `path.resolve(path.normalize(p))`).
pub fn normalize_path(p: &str) -> String {
    normalize_path_with_cwd(p, std::env::current_dir().ok().as_deref())
}

pub fn normalize_path_with_cwd(p: &str, cwd: Option<&Path>) -> String {
    let path = Path::new(p);
    let joined: PathBuf = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match cwd {
            Some(cwd) => cwd.join(path),
            None => std::env::current_dir().unwrap_or_default().join(path),
        }
    };

    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for comp in joined.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop unless at root boundary
                if !parts.is_empty() && parts.last().map(|s| !s.is_empty()).unwrap_or(false) {
                    parts.pop();
                }
            }
            Component::RootDir => parts.push(std::ffi::OsString::new()),
            other => parts.push(other.as_os_str().to_os_string()),
        }
    }

    let mut out = PathBuf::new();
    for part in parts {
        if part.is_empty() {
            out.push("/");
        } else {
            out.push(part);
        }
    }
    out.to_string_lossy().to_string()
}

/// True if `path` is inside (or equal to) `root` (files/utils.ts isWithinProject).
pub fn is_within_project(path: &str, root: &str) -> bool {
    let resolved = normalize_path(path);
    let root_n = normalize_path(root);
    resolved == root_n || resolved.starts_with(&format!("{root_n}/"))
}

/// Replace CRLF with LF (files/utils.ts normalizeLineEndings).
pub fn normalize_line_endings(s: &str) -> String {
    s.replace("\r\n", "\n")
}

/// Format content with line numbers, 1-based numbering starting at `offset`
/// (files/utils.ts formatLineNumbers; width matches the longest line number).
pub fn format_line_numbers(content: &str, offset: usize) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let max_num = offset + lines.len().saturating_sub(1);
    let width = max_num.to_string().len();
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>width$}\t{line}", offset + i, width = width))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_relative_against_cwd() {
        let cwd = Path::new("/home/u/proj");
        assert_eq!(
            normalize_path_with_cwd("src/a.rs", Some(cwd)),
            "/home/u/proj/src/a.rs"
        );
        assert_eq!(
            normalize_path_with_cwd("./src/../b.rs", Some(cwd)),
            "/home/u/proj/b.rs"
        );
        assert_eq!(
            normalize_path_with_cwd("/abs/x.rs", Some(cwd)),
            "/abs/x.rs"
        );
    }

    #[test]
    fn parent_dots_clamp_at_root() {
        assert_eq!(
            normalize_path_with_cwd("../../x", Some(Path::new("/"))),
            "/x"
        );
    }

    #[test]
    fn project_boundary() {
        assert!(is_within_project("/p/a/b.rs", "/p"));
        assert!(is_within_project("/p", "/p"));
        assert!(!is_within_project("/other/a.rs", "/p"));
        // prefix-similar but sibling directory must not match
        assert!(!is_within_project("/p2/a.rs", "/p"));
    }

    #[test]
    fn line_numbers_format() {
        let out = format_line_numbers("a\nb", 1);
        assert_eq!(out, "1\ta\n2\tb");
        let out2 = format_line_numbers("a\nb\nc", 9);
        assert_eq!(out2, " 9\ta\n10\tb\n11\tc");
    }

    #[test]
    fn crlf_normalized() {
        assert_eq!(normalize_line_endings("a\r\nb"), "a\nb");
    }
}
