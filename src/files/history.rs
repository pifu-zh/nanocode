//! File history — Rust port of `src/files/history.ts` (D1: made live).
//! Versioned backups under `~/.nanocode/file-history/{sessionId}/`, named
//! `{sha256(path)[..16]}@v{N}`; snapshots capped at 100.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::core::types::{FileHistoryBackup, FileHistorySnapshot, FileHistoryState};

const MAX_SNAPSHOTS: usize = 100;

pub struct FileHistory {
    pub base_dir: PathBuf,
}

impl FileHistory {
    pub fn new(session_id: &str) -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        FileHistory { base_dir: home.join(".nanocode").join("file-history").join(session_id) }
    }

    pub fn with_base(base_dir: PathBuf) -> Self {
        FileHistory { base_dir }
    }

    fn path_hash(file_path: &str) -> String {
        let digest = Sha256::digest(file_path.as_bytes());
        hex(&digest)[..16].to_string()
    }

    /// Backup current content before an edit (history.ts trackEdit).
    pub fn track_edit(&self, state: &mut FileHistoryState, file_path: &str) -> std::io::Result<()> {
        let resolved = crate::files::utils::normalize_path(file_path);

        // Latest known version for this file
        let mut current_version = 0u64;
        for snap in state.snapshots.iter().rev() {
            if let Some(backup) = snap.tracked_file_backups.get(&resolved) {
                current_version = backup.version;
                break;
            }
        }
        let next_version = current_version + 1;

        // Already backed up at this version in the latest snapshot?
        if let Some(latest) = state.snapshots.last() {
            if let Some(existing) = latest.tracked_file_backups.get(&resolved) {
                if existing.version == next_version {
                    return Ok(());
                }
            }
        }

        let content = std::fs::read(&resolved).ok();
        std::fs::create_dir_all(&self.base_dir)?;
        if let Some(bytes) = &content {
            let backup_name = format!("{}@v{next_version}", Self::path_hash(&resolved));
            std::fs::write(self.base_dir.join(backup_name), bytes)?;
        }

        state.tracked_files.insert(resolved);
        Ok(())
    }

    /// Snapshot all tracked files at a message boundary (makeSnapshot).
    pub fn make_snapshot(
        &self,
        state: &mut FileHistoryState,
        message_id: &str,
    ) -> std::io::Result<FileHistorySnapshot> {
        std::fs::create_dir_all(&self.base_dir)?;
        let mut tracked_file_backups: HashMap<String, FileHistoryBackup> = HashMap::new();
        let now = now_ms();

        for file_path in state.tracked_files.clone() {
            let (prev_version, prev_mtime) = state
                .snapshots
                .iter()
                .rev()
                .find_map(|snap| snap.tracked_file_backups.get(&file_path))
                .map(|b| (b.version, b.backup_time))
                .unwrap_or((0, 0));

            let current_mtime = std::fs::metadata(&file_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);

            // Unchanged since last backup → carry forward
            if prev_version > 0 && current_mtime > 0 && current_mtime <= prev_mtime {
                tracked_file_backups.insert(
                    file_path.clone(),
                    FileHistoryBackup {
                        backup_file_name: Some(format!(
                            "{}@v{prev_version}",
                            Self::path_hash(&file_path)
                        )),
                        version: prev_version,
                        backup_time: prev_mtime,
                    },
                );
                continue;
            }

            let next_version = prev_version + 1;
            let backup_name = format!("{}@v{next_version}", Self::path_hash(&file_path));
            match std::fs::read(&file_path) {
                Ok(bytes) => {
                    std::fs::write(self.base_dir.join(&backup_name), bytes)?;
                    tracked_file_backups.insert(
                        file_path.clone(),
                        FileHistoryBackup {
                            backup_file_name: Some(backup_name),
                            version: next_version,
                            backup_time: now,
                        },
                    );
                }
                Err(_) => {
                    tracked_file_backups.insert(
                        file_path.clone(),
                        FileHistoryBackup {
                            backup_file_name: None,
                            version: next_version,
                            backup_time: now,
                        },
                    );
                }
            }
        }

        state.snapshot_sequence += 1;
        let snapshot = FileHistorySnapshot {
            message_id: message_id.to_string(),
            tracked_file_backups,
            timestamp: now,
        };
        state.snapshots.push(snapshot);
        while state.snapshots.len() > MAX_SNAPSHOTS {
            state.snapshots.remove(0);
        }
        Ok(state.snapshots.last().unwrap().clone())
    }

    /// Restore files to a snapshot's state (rewind).
    pub fn rewind(
        &self,
        state: &mut FileHistoryState,
        snapshot_index: usize,
    ) -> Result<(), String> {
        if snapshot_index >= state.snapshots.len() {
            return Err(format!(
                "Invalid snapshot index {snapshot_index}. Valid range: 0..{}",
                state.snapshots.len().saturating_sub(1)
            ));
        }
        let snapshot = state.snapshots[snapshot_index].clone();
        for (file_path, backup) in &snapshot.tracked_file_backups {
            match &backup.backup_file_name {
                None => {
                    let _ = std::fs::remove_file(file_path);
                }
                Some(name) => {
                    if let Ok(content) = std::fs::read(self.base_dir.join(name)) {
                        if let Some(parent) = Path::new(file_path).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::fs::write(file_path, content);
                    }
                }
            }
        }
        state.snapshots.truncate(snapshot_index + 1);
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn setup() -> (tempfile::TempDir, FileHistory, Arc<Mutex<FileHistoryState>>) {
        let dir = tempfile::tempdir().unwrap();
        let hist = FileHistory::with_base(dir.path().join("history"));
        (dir, hist, Arc::new(Mutex::new(FileHistoryState::default())))
    }

    #[test]
    fn track_edit_creates_backup() {
        let (_dir, hist, state) = setup();
        let work = tempfile::tempdir().unwrap();
        let f = work.path().join("a.txt");
        std::fs::write(&f, "original").unwrap();

        hist.track_edit(&mut state.lock().unwrap(), f.to_str().unwrap()).unwrap();
        assert!(state.lock().unwrap().tracked_files.contains(&crate::files::utils::normalize_path(f.to_str().unwrap())));

        // backup file exists
        let backups: Vec<_> = std::fs::read_dir(&hist.base_dir).unwrap().collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read_to_string(hist.base_dir.join(backups[0].as_ref().unwrap().file_name())).unwrap(),
            "original"
        );
    }

    #[test]
    fn snapshot_and_rewind() {
        let (_dir, hist, state) = setup();
        let work = tempfile::tempdir().unwrap();
        let f = work.path().join("a.txt");

        std::fs::write(&f, "v1").unwrap();
        hist.track_edit(&mut state.lock().unwrap(), f.to_str().unwrap()).unwrap();
        hist.make_snapshot(&mut state.lock().unwrap(), "msg1").unwrap();

        std::fs::write(&f, "v2").unwrap();
        hist.make_snapshot(&mut state.lock().unwrap(), "msg2").unwrap();
        assert_eq!(state.lock().unwrap().snapshots.len(), 2);

        // Rewind to snapshot 0 → v1 content restored
        hist.rewind(&mut state.lock().unwrap(), 0).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        assert_eq!(state.lock().unwrap().snapshots.len(), 1);
    }

    #[test]
    fn rewind_invalid_index_errors() {
        let (_dir, hist, state) = setup();
        assert!(hist.rewind(&mut state.lock().unwrap(), 5).is_err());
    }

    #[test]
    fn snapshot_eviction_at_cap() {
        let (_dir, hist, state) = setup();
        for i in 0..105 {
            hist.make_snapshot(&mut state.lock().unwrap(), &format!("m{i}")).unwrap();
        }
        assert_eq!(state.lock().unwrap().snapshots.len(), 100);
    }
}
