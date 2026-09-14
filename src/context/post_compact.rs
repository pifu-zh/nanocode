//! Post-compaction file attachments — Rust port of `src/context/post-compact.ts`.
//! (Wired into the compaction flow per D1 — dead code in the TS original.)

use std::sync::Mutex;

use crate::context::token_counting::estimate_tokens;
use crate::core::types::{ContentBlock, FileState, FileStateCache, Message};

pub const MAX_FILES: usize = 5;
pub const TOKEN_BUDGET: i64 = 50_000;
pub const MAX_PER_FILE: i64 = 5_000;

struct FileEntry {
    path: String,
    timestamp: f64,
    #[allow(dead_code)]
    state: FileState,
}

fn collect_recent_files(cache: &FileStateCache) -> Vec<FileEntry> {
    let mut entries: Vec<FileEntry> = cache
        .keys()
        .into_iter()
        .filter_map(|path| {
            let state = cache.peek(&path)?;
            Some(FileEntry { path, timestamp: state.timestamp, state })
        })
        .collect();
    entries.sort_by(|a, b| b.timestamp.partial_cmp(&a.timestamp).unwrap_or(std::cmp::Ordering::Equal));
    entries
}

fn truncate_to_tokens(content: &str, max_tokens: i64) -> String {
    if estimate_tokens(content) <= max_tokens {
        return content.to_string();
    }
    let char_limit = (max_tokens * 4) as usize;
    let truncated: String = content.chars().take(char_limit).collect();
    match truncated.rfind('\n') {
        Some(idx) if idx > 0 => format!("{}\n...[truncated]", &truncated[..idx]),
        _ => format!("{truncated}...[truncated]"),
    }
}

fn format_file_attachment(path: &str, content: &str) -> String {
    format!("<file path=\"{path}\">\n{content}\n</file>")
}

/// Re-read the most recently accessed files and return them as a single
/// attachment message (within budget; skip paths already referenced in the
/// preserved messages).
pub async fn create_post_compact_attachments(
    preserved_messages: &[Message],
    file_state: &Mutex<FileStateCache>,
) -> Vec<Message> {
    let (candidates, total_cached) = {
        let cache = file_state.lock().unwrap();
        if cache.size() == 0 {
            return Vec::new();
        }
        let all = collect_recent_files(&cache);
        let size = cache.size();
        (all.into_iter().take(MAX_FILES).collect::<Vec<_>>(), size)
    };
    let _ = total_cached;

    if candidates.is_empty() {
        return Vec::new();
    }

    // Paths already referenced in preserved text — avoid duplicates
    let preserved_paths: std::collections::HashSet<String> = preserved_messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| b.as_text().map(|t| t.to_string()))
        .flat_map(|text| {
            candidates
                .iter()
                .filter(|c| text.contains(&c.path))
                .map(|c| c.path.clone())
                .collect::<Vec<_>>()
        })
        .collect();

    let mut attachments: Vec<String> = Vec::new();
    let mut total_tokens: i64 = 0;
    let per_file_limit = std::cmp::min(
        MAX_PER_FILE,
        TOKEN_BUDGET / std::cmp::min(candidates.len() as i64, MAX_FILES as i64),
    );

    for candidate in &candidates {
        if preserved_paths.contains(&candidate.path) {
            continue;
        }
        if total_tokens >= TOKEN_BUDGET {
            break;
        }
        let Ok(content) = tokio::fs::read_to_string(&candidate.path).await else {
            continue;
        };
        let remaining = TOKEN_BUDGET - total_tokens;
        let effective = std::cmp::min(per_file_limit, remaining);
        let truncated = truncate_to_tokens(&content, effective);
        total_tokens += estimate_tokens(&truncated);
        attachments.push(format_file_attachment(&candidate.path, &truncated));
    }

    if attachments.is_empty() {
        return Vec::new();
    }

    let text = format!(
        "[Post-compaction context refresh]\n\nThe following files were recently accessed in this session. They are \
provided here as context after conversation compaction so you don't need \
to re-read them unless you need the latest version.\n\n{}",
        attachments.join("\n\n")
    );

    vec![Message {
        role: crate::core::types::Role::User,
        content: vec![ContentBlock::text(text)],
        id: None,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Role;

    #[tokio::test]
    async fn attaches_recent_files_within_budget() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("one.txt");
        let f2 = dir.path().join("two.txt");
        std::fs::write(&f1, "content one\n".repeat(10)).unwrap();
        std::fs::write(&f2, "content two\n").unwrap();

        let mut cache = FileStateCache::new();
        cache_has(&mut cache, &f1, 100.0);
        cache_has(&mut cache, &f2, 200.0); // more recent

        let state = Mutex::new(cache);
        let out = create_post_compact_attachments(&[], &state).await;
        assert_eq!(out.len(), 1);
        let text = out[0].content[0].as_text().unwrap();
        assert!(text.contains("[Post-compaction context refresh]"));
        // most recent first in the attachment order
        let two_pos = text.find("two.txt").unwrap();
        let one_pos = text.find("one.txt").unwrap();
        assert!(two_pos < one_pos);
    }

    fn cache_has(cache: &mut FileStateCache, path: &std::path::Path, ts: f64) {
        cache.set(
            &path.to_string_lossy(),
            FileState { content: String::new(), timestamp: ts, offset: None, limit: None, is_partial_view: None },
        );
    }

    #[tokio::test]
    async fn skips_referenced_paths() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("referenced.txt");
        std::fs::write(&f1, "seen this").unwrap();

        let mut cache = FileStateCache::new();
        cache_has(&mut cache, &f1, 1.0);
        let state = Mutex::new(cache);

        let preserved = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::text(format!("look at {}", f1.to_string_lossy()))],
            id: None,
        }];
        let out = create_post_compact_attachments(&preserved, &state).await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn empty_cache_no_output() {
        let state = Mutex::new(FileStateCache::new());
        let out = create_post_compact_attachments(&[], &state).await;
        assert!(out.is_empty());
    }
}
