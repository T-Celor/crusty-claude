//! File checkpoints for `/rewind`.
//!
//! Layout under the checkpoint directory (one per session):
//! - `manifest.json`: the checkpoints in conversation order, with the files each touched;
//! - `blobs/<sha256>`: content-addressed copies of the file contents that were snapshotted.
//!
//! Changes made by shell commands are not tracked, as in Claude Code: only the file tools
//! call [`Checkpointer::before_change`].

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use crusty_core::checkpoint::{CheckpointSummary, Checkpointer, RewindReport};
use crusty_core::config::CheckpointConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MANIFEST: &str = "manifest.json";
const MANIFEST_TMP: &str = "manifest.json.tmp";
const BLOBS: &str = "blobs";

/// What was recorded for one file in one checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Entry {
    /// The file had this content; see `blobs/<sha256>`.
    Blob { sha256: String },
    /// The file did not exist.
    Absent,
    /// The file was changed but cannot be restored (outside the workspace, a symbolic
    /// link, a directory, or larger than the limit).
    NotTracked { reason: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Checkpoint {
    id: u32,
    label: String,
    created: String,
    message_index: usize,
    /// Relative path (with `/`) or the absolute path of an untracked file, to its entry.
    files: BTreeMap<String, Entry>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Manifest {
    /// Last id handed out; ids are never reused, even after a rewind.
    next_id: u32,
    checkpoints: Vec<Checkpoint>,
}

#[derive(Debug, Default)]
struct State {
    manifest: Manifest,
    /// The checkpoint that `before_change` records into, if one was begun.
    current: Option<u32>,
}

/// Snapshots files before the agent changes them, and restores them on `/rewind`.
pub struct FileCheckpointer {
    dir: PathBuf,
    workspace: PathBuf,
    max_file_bytes: u64,
    enabled: bool,
    state: Mutex<State>,
}

impl FileCheckpointer {
    /// Opens (or creates) the checkpoint store in `dir` for files under `workspace`.
    ///
    /// An existing manifest is loaded, so checkpoints survive a restart. No checkpoint is
    /// open until [`FileCheckpointer::begin`] is called.
    pub fn open(dir: PathBuf, workspace: PathBuf, cfg: &CheckpointConfig) -> io::Result<Self> {
        fs::create_dir_all(dir.join(BLOBS))?;
        let manifest = match fs::read(dir.join(MANIFEST)) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Manifest::default(),
            Err(e) => return Err(e),
        };
        Ok(FileCheckpointer {
            dir,
            workspace: normalize(&workspace),
            max_file_bytes: cfg.max_file_bytes,
            enabled: cfg.enabled,
            state: Mutex::new(State { manifest, current: None }),
        })
    }

    /// Starts a checkpoint for one user prompt and returns its id; `None` when disabled.
    ///
    /// A failed manifest write is logged, not returned: the checkpoint still works in memory.
    fn begin_checkpoint(&self, label: &str, message_index: usize) -> Option<u32> {
        if !self.enabled {
            return None;
        }
        let mut st = self.lock();
        st.manifest.next_id += 1;
        let id = st.manifest.next_id;
        let label = crate::truncate_chars(&label.split_whitespace().collect::<Vec<_>>().join(" "), 200);
        st.manifest.checkpoints.push(Checkpoint {
            id,
            label,
            created: chrono::Utc::now().to_rfc3339(),
            message_index,
            files: BTreeMap::new(),
        });
        st.current = Some(id);
        if let Err(e) = save(&self.dir, &st.manifest) {
            tracing::warn!(error = %e, "could not save the checkpoint manifest");
        }
        Some(id)
    }

    /// The checkpoints in conversation order.
    fn list_checkpoints(&self) -> Vec<CheckpointSummary> {
        let st = self.lock();
        st.manifest
            .checkpoints
            .iter()
            .map(|c| CheckpointSummary {
                id: c.id,
                label: c.label.clone(),
                files: c.files.len(),
                created: c.created.clone(),
                message_index: c.message_index,
            })
            .collect()
    }

    /// Restores every file touched in checkpoints `id..=latest` to its state when checkpoint
    /// `id` began, then drops those checkpoints. The earliest snapshot of each path wins.
    ///
    /// Untracked files cannot be restored and are listed in `failures`. `None` when no
    /// checkpoint has that id.
    fn rewind_to(&self, id: u32) -> Option<RewindReport> {
        let mut st = self.lock();
        let pos = st.manifest.checkpoints.iter().position(|c| c.id == id)?;
        let mut earliest: BTreeMap<String, Entry> = BTreeMap::new();
        for cp in &st.manifest.checkpoints[pos..] {
            for (key, entry) in &cp.files {
                earliest.entry(key.clone()).or_insert_with(|| entry.clone());
            }
        }
        let mut report = RewindReport::default();
        for (key, entry) in &earliest {
            match self.restore(key, entry) {
                Ok(()) => report.files_restored += 1,
                Err(e) => report.failures.push(format!("{key}: {e}")),
            }
        }
        st.manifest.checkpoints.truncate(pos);
        st.current = None;
        if let Err(e) = save(&self.dir, &st.manifest) {
            report.failures.push(format!("could not save the checkpoint manifest: {e}"));
        }
        Some(report)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records the pre-change state of `path` in the current checkpoint, once.
    fn record(&self, path: &Path) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        let target = self.target(path);
        let mut st = self.lock();
        let id = st.current.ok_or("no checkpoint is open")?;
        if current_mut(&mut st.manifest, id).is_some_and(|cp| cp.files.contains_key(&target.key)) {
            return Ok(());
        }
        let entry = self.snapshot(&target).map_err(|e| e.to_string())?;
        if let Some(cp) = current_mut(&mut st.manifest, id) {
            cp.files.insert(target.key.clone(), entry);
        }
        if let Err(e) = save(&self.dir, &st.manifest) {
            if let Some(cp) = current_mut(&mut st.manifest, id) {
                cp.files.remove(&target.key);
            }
            return Err(format!("could not save the checkpoint manifest: {e}"));
        }
        Ok(())
    }

    /// Reads the current state of a target, storing its content as a blob if needed.
    fn snapshot(&self, target: &Target) -> io::Result<Entry> {
        if let Some(reason) = &target.blocked {
            return Ok(Entry::NotTracked { reason: reason.clone() });
        }
        let meta = match fs::symlink_metadata(&target.abs) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Entry::Absent),
            Err(e) => return Err(e),
        };
        if meta.is_dir() {
            return Ok(Entry::NotTracked { reason: "a directory".into() });
        }
        if meta.len() > self.max_file_bytes {
            return Ok(Entry::NotTracked { reason: format!("larger than {} bytes", self.max_file_bytes) });
        }
        let bytes = fs::read(&target.abs)?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let blob = self.dir.join(BLOBS).join(&sha256);
        if !blob.exists() {
            let tmp = blob.with_extension("tmp");
            fs::write(&tmp, &bytes)?;
            fs::rename(&tmp, &blob)?;
        }
        Ok(Entry::Blob { sha256 })
    }

    /// Puts one file back to `entry`.
    fn restore(&self, key: &str, entry: &Entry) -> Result<(), String> {
        let abs = match entry {
            Entry::NotTracked { reason } => return Err(format!("not tracked ({reason}), cannot be restored")),
            _ => self.target(&self.workspace.join(key)),
        };
        if let Some(reason) = &abs.blocked {
            return Err(format!("not restored ({reason})"));
        }
        match entry {
            Entry::Blob { sha256 } => {
                if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err("corrupt blob reference".into());
                }
                let bytes = fs::read(self.dir.join(BLOBS).join(sha256)).map_err(|e| format!("blob unreadable: {e}"))?;
                if let Some(parent) = abs.abs.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                fs::write(&abs.abs, bytes).map_err(|e| e.to_string())
            }
            Entry::Absent => match fs::remove_file(&abs.abs) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.to_string()),
            },
            Entry::NotTracked { .. } => Err("not tracked".into()),
        }
    }

    /// Resolves `path` against the workspace. Paths outside it, or through a symbolic link,
    /// are blocked; the key is then the absolute path.
    fn target(&self, path: &Path) -> Target {
        let abs = normalize(&if path.is_absolute() { path.to_path_buf() } else { self.workspace.join(path) });
        let Ok(rel) = abs.strip_prefix(&self.workspace) else {
            return Target { key: abs.to_string_lossy().into_owned(), abs, blocked: Some("outside the workspace".into()) };
        };
        let key = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
        let mut blocked = if rel.as_os_str().is_empty() {
            Some("the workspace root".to_owned())
        } else if rel.components().next().is_some_and(|c| c.as_os_str().eq_ignore_ascii_case(".git")) {
            // Rewinding into the repository could plant hooks or rewrite its state.
            Some("inside .git".to_owned())
        } else {
            None
        };
        let mut cur = self.workspace.clone();
        for c in rel.components() {
            cur.push(c.as_os_str());
            if fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
                blocked = Some("a symbolic link".into());
                break;
            }
        }
        Target { key, abs, blocked }
    }
}

/// A resolved path: the manifest key, the absolute path and why it cannot be tracked.
struct Target {
    key: String,
    abs: PathBuf,
    blocked: Option<String>,
}

impl Checkpointer for FileCheckpointer {
    fn before_change(&self, path: &Path) -> Result<(), String> {
        self.record(path)
    }

    fn begin(&self, label: &str, message_index: usize) -> Option<u32> {
        self.begin_checkpoint(label, message_index)
    }

    fn list(&self) -> Vec<CheckpointSummary> {
        self.list_checkpoints()
    }

    fn rewind(&self, id: u32) -> Result<RewindReport, String> {
        self.rewind_to(id).ok_or_else(|| format!("no checkpoint {id}"))
    }
}

/// Removes checkpoint directories beyond the newest `keep`. `root` holds one checkpoint
/// directory per session; the newest is judged by modification time. Returns how many were
/// removed. A missing `root` removes nothing.
pub fn prune_sessions(root: &Path, keep: usize) -> io::Result<usize> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut dirs: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push((entry.metadata()?.modified()?, entry.path()));
        }
    }
    dirs.sort_by(|a, b| b.cmp(a));
    let mut removed = 0;
    for (_, path) in dirs.into_iter().skip(keep) {
        fs::remove_dir_all(&path)?;
        removed += 1;
    }
    Ok(removed)
}

fn current_mut(manifest: &mut Manifest, id: u32) -> Option<&mut Checkpoint> {
    manifest.checkpoints.iter_mut().find(|c| c.id == id)
}

/// Writes the manifest atomically: a temporary file, then a rename over the old one.
fn save(dir: &Path, manifest: &Manifest) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
    let tmp = dir.join(MANIFEST_TMP);
    fs::write(&tmp, json)?;
    fs::rename(&tmp, dir.join(MANIFEST))
}

/// Resolves `.` and `..` without touching the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(max_file_bytes: u64) -> CheckpointConfig {
        CheckpointConfig { enabled: true, max_file_bytes, keep_sessions: 20 }
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, text).expect("write");
    }

    #[test]
    fn snapshot_is_taken_once_per_checkpoint() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join("a.txt"), "v0");
        let cp = FileCheckpointer::open(tmp.path().join("cp"), ws.clone(), &config(1024)).expect("open");
        cp.begin("first prompt", 0);
        cp.before_change(&ws.join("a.txt")).expect("snapshot");
        write(&ws.join("a.txt"), "v1");
        cp.before_change(&ws.join("a.txt")).expect("second snapshot is a no-op");
        write(&ws.join("a.txt"), "v2");
        let list = cp.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].files, 1);
        assert_eq!(list[0].label, "first prompt");
        let report = cp.rewind_to(1).expect("checkpoint 1");
        assert!(report.failures.is_empty());
        assert_eq!(fs::read_to_string(ws.join("a.txt")).expect("read"), "v0");
    }

    #[test]
    fn rewind_restores_modified_recreated_and_removes_created_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join("a.txt"), "orig");
        write(&ws.join("c.txt"), "c0");
        let cp = FileCheckpointer::open(tmp.path().join("cp"), ws.clone(), &config(1024)).expect("open");

        cp.begin("one", 0);
        cp.before_change(&ws.join("a.txt")).expect("a");
        write(&ws.join("a.txt"), "a1");

        cp.begin("two", 2);
        cp.before_change(&ws.join("b.txt")).expect("b");
        write(&ws.join("b.txt"), "b2");
        cp.before_change(&ws.join("c.txt")).expect("c");
        fs::remove_file(ws.join("c.txt")).expect("remove c");

        cp.begin("three", 4);
        cp.before_change(&ws.join("b.txt")).expect("b again");
        write(&ws.join("b.txt"), "b3");
        cp.before_change(&ws.join("a.txt")).expect("a again");
        write(&ws.join("a.txt"), "a3");

        let report = cp.rewind_to(1).expect("checkpoint 1");
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.files_restored, 3);
        assert_eq!(fs::read_to_string(ws.join("a.txt")).expect("a"), "orig");
        assert_eq!(fs::read_to_string(ws.join("c.txt")).expect("c"), "c0");
        assert!(!ws.join("b.txt").exists());
        assert!(cp.list().is_empty());
    }

    #[test]
    fn untracked_files_are_reported() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join("big.txt"), "0123456789");
        write(&tmp.path().join("outside.txt"), "x");
        let cp = FileCheckpointer::open(tmp.path().join("cp"), ws.clone(), &config(4)).expect("open");
        cp.begin("prompt", 0);
        cp.before_change(&ws.join("big.txt")).expect("big");
        cp.before_change(&tmp.path().join("outside.txt")).expect("outside");
        assert_eq!(cp.list()[0].files, 2);
        let report = cp.rewind_to(1).expect("checkpoint 1");
        assert_eq!(report.failures.len(), 2, "{:?}", report.failures);
        assert!(report.failures.iter().any(|f| f.contains("larger than")));
        assert!(report.failures.iter().any(|f| f.contains("outside the workspace")));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&tmp.path().join("secret.txt"), "s");
        fs::create_dir_all(&ws).expect("ws");
        std::os::unix::fs::symlink(tmp.path().join("secret.txt"), ws.join("link.txt")).expect("symlink");
        let cp = FileCheckpointer::open(tmp.path().join("cp"), ws.clone(), &config(1024)).expect("open");
        cp.begin("prompt", 0);
        cp.before_change(&ws.join("link.txt")).expect("link");
        let report = cp.rewind_to(1).expect("checkpoint 1");
        assert!(report.failures.iter().any(|f| f.contains("symbolic link")));
        assert_eq!(fs::read_to_string(tmp.path().join("secret.txt")).expect("secret"), "s");
    }

    #[test]
    fn git_directory_is_never_snapshotted_or_restored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join(".git/hooks/pre-commit"), "orig");
        let cp = FileCheckpointer::open(tmp.path().join("cp"), ws.clone(), &config(1024)).expect("open");
        cp.begin("prompt", 0);
        cp.before_change(&ws.join(".git/hooks/pre-commit")).expect("recorded as untracked");
        write(&ws.join(".git/hooks/pre-commit"), "evil");
        let report = cp.rewind_to(1).expect("checkpoint 1");
        assert!(report.failures.iter().any(|f| f.contains("inside .git")), "{:?}", report.failures);
        assert_eq!(fs::read_to_string(ws.join(".git/hooks/pre-commit")).expect("read"), "evil");
        assert!(!fs::read_dir(tmp.path().join("cp/blobs")).expect("blobs").any(|e| e.is_ok()));
    }

    #[test]
    fn reopen_reloads_checkpoints() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("ws");
        write(&ws.join("a.txt"), "before");
        let dir = tmp.path().join("cp");
        {
            let cp = FileCheckpointer::open(dir.clone(), ws.clone(), &config(1024)).expect("open");
            cp.begin("persisted", 7);
            cp.before_change(&ws.join("a.txt")).expect("snapshot");
        }
        write(&ws.join("a.txt"), "after");
        let cp = FileCheckpointer::open(dir, ws.clone(), &config(1024)).expect("reopen");
        let list = cp.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].message_index, 7);
        assert!(cp.rewind_to(1).expect("checkpoint 1").failures.is_empty());
        assert_eq!(fs::read_to_string(ws.join("a.txt")).expect("read"), "before");
    }

    #[test]
    fn prune_keeps_newest_sessions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for name in ["s1", "s2", "s3"] {
            fs::create_dir_all(tmp.path().join(name)).expect("mkdir");
        }
        assert_eq!(prune_sessions(tmp.path(), 2).expect("prune"), 1);
        assert_eq!(fs::read_dir(tmp.path()).expect("read").count(), 2);
        assert_eq!(prune_sessions(&tmp.path().join("missing"), 2).expect("missing"), 0);
    }
}
