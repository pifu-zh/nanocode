//! Atomic file write — unified port of the three inline implementations in
//! edit.ts / write.ts / notebook-edit.ts plus the permission-preserving
//! variant from files/atomic-write.ts (decision D1: one module, not four).
//!
//! Strategy: write `.nanocode-tmp-{uuid}` in the same directory, preserve the
//! original mode if the target exists, rename over the target; clean up the
//! temp file on failure.

use std::io::Write;
use std::path::Path;

/// Synchronous atomic write (matches the tool implementations' behavior).
pub fn atomic_write_sync(file_path: &Path, content: &str) -> std::io::Result<()> {
    let dir = file_path
        .parent()
        .ok_or_else(|| std::io::Error::other("path has no parent"))?;
    std::fs::create_dir_all(dir)?;

    let original_mode = {
        #[cfg(unix)]
        {
            std::fs::metadata(file_path).ok().map(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.permissions().mode()
            })
        }
        #[cfg(not(unix))]
        {
            None::<u32>
        }
    };

    let temp_path = dir.join(format!(".nanocode-tmp-{}", uuid::Uuid::new_v4()));

    let write_result = (|| -> std::io::Result<()> {
        {
            let mut f = std::fs::File::create(&temp_path)?;
            f.write_all(content.as_bytes())?;
        }
        #[cfg(unix)]
        if let Some(mode) = original_mode {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(mode));
        }
        std::fs::rename(&temp_path, file_path)?;
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }
    Ok(())
}

/// Async wrapper.
pub async fn atomic_write(file_path: &Path, content: &str) -> std::io::Result<()> {
    let path = file_path.to_path_buf();
    let content = content.to_string();
    tokio::task::spawn_blocking(move || atomic_write_sync(&path, &content))
        .await
        .expect("blocking task")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_new_file_and_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("deep/nested/file.txt");
        atomic_write_sync(&target, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
    }

    #[test]
    fn overwrite_is_atomic_no_temp_left() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("f.txt");
        std::fs::write(&target, "old").unwrap();
        atomic_write_sync(&target, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".nanocode-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn preserves_permissions_on_overwrite() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("f.sh");
        std::fs::write(&target, "#!/bin/sh").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        atomic_write_sync(&target, "#!/bin/sh\necho").unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn parent_directory_missing_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a/b/c.txt");
        atomic_write_sync(&target, "x").unwrap();
        assert!(target.exists());
    }
}
