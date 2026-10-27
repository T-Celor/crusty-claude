//! Read-before-write bookkeeping: an existing file may be changed only if this session read
//! it and it has not changed on disk since.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use crusty_core::tool::ToolError;

use crate::sandbox::display_path;

/// Modification times of files as the session last saw them, keyed by canonical path.
#[derive(Debug, Default)]
pub struct FileTracker {
    seen: Mutex<HashMap<PathBuf, SystemTime>>,
}

impl FileTracker {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<PathBuf, SystemTime>> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Remembers the file as it is now.
    pub fn record_read(&self, path: &Path) {
        self.remember(path);
    }

    /// Remembers the file after this session wrote it, so the next edit is not blocked.
    pub fn record_write(&self, path: &Path) {
        self.remember(path);
    }

    fn remember(&self, path: &Path) {
        if let Ok(modified) = modified_time(path) {
            self.lock().insert(path.to_path_buf(), modified);
        }
    }

    /// Fails for an existing file that was not read in this session or changed since then.
    /// New files pass.
    pub fn check_can_write(&self, path: &Path) -> Result<(), ToolError> {
        if !path.exists() {
            return Ok(());
        }
        let seen = self.lock().get(path).copied().ok_or_else(|| {
            ToolError::Failed(format!("{} exists but was not read in this session; Read it before changing it", display_path(path)))
        })?;
        if modified_time(path)? > seen {
            return Err(ToolError::Failed(format!("{} changed since it was read; Read it again", display_path(path))));
        }
        Ok(())
    }
}

fn modified_time(path: &Path) -> std::io::Result<SystemTime> {
    std::fs::metadata(path)?.modified()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn read_file_can_be_written_and_new_file_always_can() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x").unwrap();
        let tracker = FileTracker::new();
        assert!(tracker.check_can_write(&path).is_err());
        tracker.record_read(&path);
        assert!(tracker.check_can_write(&path).is_ok());
        assert!(tracker.check_can_write(&dir.path().join("new.txt")).is_ok());
    }

    #[test]
    fn file_changed_on_disk_after_read_is_refused() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x").unwrap();
        let tracker = FileTracker::new();
        tracker.record_read(&path);
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(&path, "changed").unwrap();
        assert!(matches!(tracker.check_can_write(&path), Err(ToolError::Failed(msg)) if msg.contains("changed since")));
        tracker.record_write(&path);
        assert!(tracker.check_can_write(&path).is_ok());
    }
}
