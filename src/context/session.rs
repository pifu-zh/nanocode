//! Session persistence — Rust port of `src/context/session.ts`.
//! JSONL transcripts under `~/.nanocode/sessions/{id}/` + meta.json.
//! As a store struct (no module singletons, RUST_DESIGN §1.7).

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::core::types::{Message, Role, SessionEntry, SessionEntryType};

pub const TRANSCRIPT_FILE: &str = "transcript.jsonl";
pub const META_FILE: &str = "meta.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub cwd: String,
    pub message_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "summary")]
    pub summary: Option<String>,
}

/// JSONL/meta field names follow the TS meta.json camelCase format.
#[derive(Serialize, Deserialize)]
struct MetaFile {
    id: String,
    #[serde(rename = "createdAt")]
    created_at: u64,
    #[serde(rename = "updatedAt")]
    updated_at: u64,
    cwd: String,
    #[serde(rename = "messageCount")]
    message_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub cwd: String,
    pub message_count: u64,
    pub summary: Option<String>,
}

pub struct SessionStore {
    base_dir: PathBuf,
}

impl Default for SessionStore {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        SessionStore { base_dir: home.join(".nanocode").join("sessions") }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl SessionStore {
    pub fn with_base(base_dir: PathBuf) -> Self {
        SessionStore { base_dir }
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(session_id)
    }

    fn transcript_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join(TRANSCRIPT_FILE)
    }

    fn meta_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join(META_FILE)
    }

    pub fn new_session_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// Lazily initialize a session directory + meta (no empty sessions).
    pub fn init_session(&self, session_id: &str, cwd: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(self.session_dir(session_id))?;
        self.write_meta(
            session_id,
            MetaFile {
                id: session_id.to_string(),
                created_at: now_ms(),
                updated_at: now_ms(),
                cwd: cwd.to_string(),
                message_count: 0,
                summary: None,
            },
        )
    }

    fn write_meta(&self, session_id: &str, mut meta: MetaFile) -> std::io::Result<()> {
        meta.updated_at = now_ms();
        let path = self.meta_path(session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&meta)? + "\n";
        std::fs::write(path, json)
    }

    fn read_meta(&self, session_id: &str) -> Option<MetaFile> {
        let raw = std::fs::read_to_string(self.meta_path(session_id)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Append one message; creates the session on first save (lazy init).
    pub fn save_message(&self, session_id: &str, message: &Message, cwd: &str) -> std::io::Result<()> {
        if !self.session_dir(session_id).exists() {
            self.init_session(session_id, cwd)?;
        }
        let entry = SessionEntry {
            entry_type: if message.role == Role::User {
                SessionEntryType::User
            } else {
                SessionEntryType::Assistant
            },
            message: message.clone(),
            timestamp: now_ms(),
            id: uuid::Uuid::new_v4().to_string(),
        };
        let line = serde_json::to_string(&entry)? + "\n";
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.transcript_path(session_id))?;
        f.write_all(line.as_bytes())?;
        self.write_meta(session_id, MetaFile {
            message_count: self.count_entries(session_id),
            ..self.read_meta(session_id).unwrap_or(MetaFile {
                id: session_id.to_string(),
                created_at: now_ms(),
                updated_at: now_ms(),
                cwd: cwd.to_string(),
                message_count: 0,
                summary: None,
            })
        })
    }

    fn count_entries(&self, session_id: &str) -> u64 {
        std::fs::read_to_string(self.transcript_path(session_id))
            .map(|raw| raw.lines().filter(|l| !l.trim().is_empty()).count() as u64)
            .unwrap_or(0)
    }

    /// Load all messages; empty if the session doesn't exist. Malformed
    /// lines are skipped (TS parity).
    pub fn load_session(&self, session_id: &str) -> Vec<Message> {
        let Ok(raw) = std::fs::read_to_string(self.transcript_path(session_id)) else {
            return Vec::new();
        };
        raw.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<SessionEntry>(l).ok())
            .map(|e| e.message)
            .collect()
    }

    pub fn session_exists(&self, session_id: &str) -> bool {
        self.session_dir(session_id).exists()
    }

    /// List sessions sorted by most recently updated.
    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        let Ok(entries) = std::fs::read_dir(&self.base_dir) else {
            return Vec::new();
        };
        let mut out: Vec<SessionInfo> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| {
                let id = e.file_name().to_string_lossy().to_string();
                self.read_meta(&id).map(|meta| SessionInfo {
                    id: meta.id,
                    created_at: meta.created_at,
                    updated_at: meta.updated_at,
                    cwd: meta.cwd,
                    message_count: meta.message_count,
                    summary: meta.summary,
                })
            })
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        out
    }

    pub fn update_summary(&self, session_id: &str, summary: &str) -> std::io::Result<()> {
        let meta = self.read_meta(session_id).unwrap_or(MetaFile {
            id: session_id.to_string(),
            created_at: now_ms(),
            updated_at: now_ms(),
            cwd: String::new(),
            message_count: 0,
            summary: None,
        });
        self.write_meta(session_id, MetaFile { summary: Some(summary.to_string()), ..meta })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::ContentBlock;

    fn store() -> (SessionStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (SessionStore::with_base(dir.path().to_path_buf()), dir)
    }

    #[test]
    fn lazy_init_and_roundtrip() {
        let (store, _dir) = store();
        let id = SessionStore::new_session_id();
        assert!(!store.session_exists(&id));

        store
            .save_message(&id, &Message::user_text("hello"), "/proj")
            .unwrap();
        assert!(store.session_exists(&id)); // created lazily on first save

        store
            .save_message(
                &id,
                &Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::text("hi there")],
                    id: None,
                },
                "/proj",
            )
            .unwrap();

        let msgs = store.load_session(&id);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].assistant_text(), "hi there");
    }

    #[test]
    fn load_missing_returns_empty() {
        let (store, _dir) = store();
        assert!(store.load_session("nonexistent").is_empty());
    }

    #[test]
    fn malformed_lines_skipped() {
        let (store, _dir) = store();
        let id = "test-session";
        std::fs::create_dir_all(store.session_dir(id)).unwrap();
        let good = serde_json::to_string(&SessionEntry {
            entry_type: SessionEntryType::User,
            message: Message::user_text("ok"),
            timestamp: 1,
            id: "e1".into(),
        })
        .unwrap();
        std::fs::write(
            store.transcript_path(id),
            format!("NOT JSON\n{good}\n\n{{\"broken\": true}}\n"),
        )
        .unwrap();
        let msgs = store.load_session(id);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].assistant_text(), "ok");
    }

    #[test]
    fn list_sorted_by_updated() {
        let (store, _dir) = store();
        let a = "session-a";
        let b = "session-b";
        store.save_message(a, &Message::user_text("x"), "/a").unwrap();
        store.save_message(b, &Message::user_text("y"), "/b").unwrap();
        // Force b to appear older
        let meta_path = store.meta_path(b);
        let mut meta: MetaFile =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta.updated_at = 1;
        std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

        let sessions = store.list_sessions();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, a); // most recent first
        assert_eq!(sessions[1].id, b);
    }

    #[test]
    fn summary_roundtrip() {
        let (store, _dir) = store();
        let id = "s";
        store.save_message(id, &Message::user_text("x"), "/p").unwrap();
        store.update_summary(id, "the summary").unwrap();
        let sessions = store.list_sessions();
        assert_eq!(sessions[0].summary.as_deref(), Some("the summary"));
    }
}
