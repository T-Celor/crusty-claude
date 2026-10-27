//! Confinement of Bash commands: the sandbox setup shared by the shell tools, and the command
//! each call actually spawns.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crusty_core::config::{SandboxConfig, SandboxMode};
use crusty_core::tool::{ToolContext, ToolError};
use crusty_sandbox::{Backend, Detection, Policy, SandboxError, wrap};
use tokio::process::Child;

#[cfg(windows)]
use crusty_sandbox::JobGuard;

/// Set once the "running unconfined" warning has been sent, so it is not repeated per command.
static UNCONFINED_WARNED: AtomicBool = AtomicBool::new(false);

/// Largest protected file whose content is kept for restoring.
// shortcut: 1 MiB per file and 16 MiB in all are kept, larger files are only reported, upgrade to hashing them
const PROTECTED_MAX_BYTES: u64 = 1024 * 1024;
/// Total bytes of protected content kept across all files.
const PROTECTED_BUDGET_BYTES: u64 = 16 * 1024 * 1024;
/// Most files listed under protected paths; beyond this the walk stops.
const PROTECTED_MAX_FILES: usize = 2000;

/// How Bash commands are confined: the `[sandbox]` settings, the detected backend and this
/// executable, which the Landlock backend re-enters.
#[derive(Debug, Clone)]
pub struct SandboxSetup {
    /// The `[sandbox]` configuration.
    pub config: SandboxConfig,
    /// The backend found on this machine.
    pub detection: Detection,
    /// This executable, used by the Landlock backend.
    pub self_exe: PathBuf,
}

/// The program and arguments one Bash call spawns, and whether a Windows job must adopt the child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// Executable to spawn.
    pub program: String,
    /// Arguments for `program`.
    pub args: Vec<String>,
    /// Whether the child joins a kill-on-close job after spawning.
    job: bool,
    /// Protected paths inside writable directories, which the Landlock backend cannot make
    /// read-only. Their files are snapshotted and restored around the command.
    protected: Vec<PathBuf>,
}

/// The outcome of [`settle`]: which protected paths were restored, and a note for the output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    /// Protected paths the command changed and that were put back.
    pub restored: Vec<PathBuf>,
    /// Lines to append to the command output.
    pub message: String,
}

/// Protected files as they were before a command ran (Landlock only). Files that did not exist
/// are recognised at restore time as files the command created.
#[derive(Debug)]
pub struct Protected {
    roots: Vec<PathBuf>,
    /// Files present before the command, with their content.
    files: BTreeMap<PathBuf, Vec<u8>>,
    /// Files present before the command that are not kept (too large, unreadable, or over the
    /// budget). They are never deleted, and cannot be restored.
    untracked: BTreeSet<PathBuf>,
    /// Whether the walk listed every file. When it did not, nothing is deleted.
    complete: bool,
}

impl Protected {
    /// Records the files under `roots` (files or directories, symbolic links skipped).
    pub fn capture(roots: Vec<PathBuf>) -> Self {
        let (paths, complete) = list_files(&roots);
        let mut files = BTreeMap::new();
        let mut untracked = BTreeSet::new();
        let mut budget = PROTECTED_BUDGET_BYTES;
        for path in paths {
            let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(u64::MAX);
            if len > PROTECTED_MAX_BYTES || len > budget {
                untracked.insert(path);
                continue;
            }
            match fs::read(&path) {
                Ok(bytes) => {
                    budget -= len;
                    files.insert(path, bytes);
                }
                Err(_) => {
                    untracked.insert(path);
                }
            }
        }
        Protected { roots, files, untracked, complete }
    }

    /// Puts back every tracked file the command changed or removed, deletes the files it created
    /// and returns the paths touched.
    pub fn restore(&self) -> Vec<PathBuf> {
        let mut changed = Vec::new();
        if self.complete {
            for path in list_files(&self.roots).0 {
                if self.files.contains_key(&path) || self.untracked.contains(&path) {
                    continue;
                }
                if fs::remove_file(&path).is_ok() {
                    changed.push(path);
                }
            }
        }
        for (path, bytes) in &self.files {
            // A symbolic link planted at (or above) a protected path is removed first, so the
            // write below can never follow it out of the workspace.
            let linked = self.unlink_planted_links(path);
            if !linked && fs::read(path).ok().as_deref() == Some(bytes.as_slice()) {
                continue;
            }
            let written = path.parent().map_or(Ok(()), fs::create_dir_all).and_then(|()| {
                if self.has_link_ancestor(path) {
                    Err(std::io::Error::other("a symbolic link is in the way"))
                } else {
                    fs::write(path, bytes)
                }
            });
            if written.is_ok() || linked {
                changed.push(path.clone());
            }
        }
        changed
    }

    /// Removes symbolic links at `path` or at any of its ancestors below the protected roots.
    /// True when one was removed.
    fn unlink_planted_links(&self, path: &Path) -> bool {
        let mut removed = false;
        let root = self.roots.iter().find(|r| path.starts_with(r)).cloned().unwrap_or_default();
        let mut current = Some(path);
        while let Some(p) = current {
            if fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
                removed |= fs::remove_file(p).or_else(|_| fs::remove_dir(p)).is_ok();
            }
            if p == root.as_path() {
                break;
            }
            current = p.parent().filter(|parent| parent.starts_with(&root));
        }
        removed
    }

    /// Whether a symbolic link sits on the way to `path` (one that could not be removed).
    fn has_link_ancestor(&self, path: &Path) -> bool {
        path.ancestors()
            .skip(1)
            .take_while(|p| self.roots.iter().any(|r| p.starts_with(r)))
            .any(|p| fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()))
    }

    /// A note when some protected files could not be tracked, for the output.
    fn partial_note(&self) -> Option<String> {
        let mut reasons = Vec::new();
        if !self.untracked.is_empty() {
            reasons.push(format!("{} file(s) too large, unreadable or over the budget", self.untracked.len()));
        }
        if !self.complete {
            reasons.push(format!("the walk stopped after {PROTECTED_MAX_FILES} files"));
        }
        (!reasons.is_empty()).then(|| format!("[sandbox] protected paths are only partly tracked: {}", reasons.join("; ")))
    }
}

/// Lists the regular files under `roots`, at most `PROTECTED_MAX_FILES` of them. The flag is
/// false when the limit cut the walk short. Symbolic links are not followed.
fn list_files(roots: &[PathBuf]) -> (Vec<PathBuf>, bool) {
    let mut files = Vec::new();
    let mut stack: Vec<PathBuf> = roots.to_vec();
    while let Some(path) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = fs::read_dir(&path) {
                stack.extend(entries.flatten().map(|e| e.path()));
            }
        } else if meta.is_file() || meta.file_type().is_symlink() {
            // Links are listed (never followed): one created by the command is then removed.
            if files.len() == PROTECTED_MAX_FILES {
                return (files, false);
            }
            files.push(path);
        }
    }
    (files, true)
}

/// Restores the protected files after a command ran, and reports what was put back. `None`
/// when there is nothing to say.
pub fn settle(protected: Option<Protected>) -> Option<Settled> {
    let protected = protected?;
    let restored = protected.restore();
    let mut lines = Vec::new();
    if !restored.is_empty() {
        let names: Vec<String> = restored.iter().map(|p| p.display().to_string()).collect();
        lines.push(format!("[sandbox] restored protected paths changed by the command: {}", names.join(", ")));
    }
    lines.extend(protected.partial_note());
    (!lines.is_empty()).then(|| Settled { restored, message: lines.join("\n") })
}

/// What must stay alive while a confined process runs. Dropping it ends a Windows job and with
/// it the process tree.
#[derive(Default)]
pub struct Keep {
    #[cfg(windows)]
    _job: Option<JobGuard>,
}

/// Decides how `program args` is spawned for one call. Runs it as it is when `unsandboxed`
/// is set (the call's `dangerouslyDisableSandbox`) or there is no setup or the mode is `off`.
/// An unavailable backend fails the call when `fail_if_unavailable` is set, and otherwise runs
/// unconfined with a warning once per process.
pub fn launch(
    setup: Option<&SandboxSetup>,
    ctx: &ToolContext,
    unsandboxed: bool,
    program: &str,
    args: &[String],
) -> Result<Launch, ToolError> {
    let plain = Launch { program: program.to_owned(), args: args.to_vec(), job: false, protected: Vec::new() };
    let Some(setup) = setup.filter(|s| s.config.mode != SandboxMode::Off && !unsandboxed) else {
        return Ok(plain);
    };
    let policy = Policy::from_config(&setup.config, &ctx.workspace, &ctx.workspace);
    let wrapped = wrap(&policy, &setup.detection, program, args, &setup.self_exe).map_err(|e| match e {
        SandboxError::Unavailable(detail) => ToolError::Failed(format!(
            "sandbox unavailable, command not run: {detail}. Set [sandbox] fail_if_unavailable = false to run commands unconfined"
        )),
        other => ToolError::Failed(other.to_string()),
    })?;
    if !setup.detection.available && !UNCONFINED_WARNED.swap(true, Ordering::Relaxed) {
        ctx.events.warn(format!("sandbox unavailable, Bash commands run unconfined: {}", setup.detection.detail));
    }
    // Without a known executable the Windows backend cannot use its helper and joins the job after spawning.
    let job = setup.detection.backend == Backend::WindowsJob && setup.self_exe.as_os_str().is_empty();
    // Landlock cannot make protected paths read-only inside writable roots, and bubblewrap can only
    // ro-bind paths that already exist (a missing `.mcp.json` could be created), so for both the
    // protected paths are checked and restored after the run.
    let protected: Vec<PathBuf> = match setup.detection.backend {
        Backend::Landlock | Backend::Bubblewrap => {
            policy.protected.iter().filter(|p| policy.writable_dirs().iter().any(|d| p.starts_with(d))).cloned().collect()
        }
        _ => Vec::new(),
    };
    Ok(Launch { program: wrapped.program, args: wrapped.args, job, protected })
}

impl Launch {
    /// Snapshots the protected files before the command starts; `None` unless the Landlock
    /// backend is in use. Pass the result to [`settle`] once the command has ended.
    pub fn protect(&self) -> Option<Protected> {
        (!self.protected.is_empty()).then(|| Protected::capture(self.protected.clone()))
    }

    /// Confines `child` once it has spawned: on Windows it joins a kill-on-close job, and the
    /// returned guard must live as long as the child. The child can start its own children
    /// before this runs, so those may escape the job; the spawn window is small.
    pub fn adopt(&self, child: &Child) -> io::Result<Keep> {
        if !self.job {
            return Ok(Keep::default());
        }
        adopt_job(child)
    }
}

#[cfg(windows)]
fn adopt_job(child: &Child) -> io::Result<Keep> {
    let job = JobGuard::new()?;
    if let Some(handle) = child.raw_handle() {
        job.assign_handle(handle)?;
    }
    Ok(Keep { _job: Some(job) })
}

#[cfg(not(windows))]
fn adopt_job(_child: &Child) -> io::Result<Keep> {
    Ok(Keep::default())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn bubblewrap_setup(network: bool) -> SandboxSetup {
        SandboxSetup {
            config: SandboxConfig { network, ..SandboxConfig::default() },
            detection: Detection {
                backend: Backend::Bubblewrap,
                available: true,
                confines_writes: true,
                blocks_network: true,
                detail: "bubblewrap at /usr/bin/bwrap".into(),
            },
            self_exe: PathBuf::from("/usr/bin/crusty"),
        }
    }

    fn ctx() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::for_tests(dir.path());
        (dir, ctx)
    }

    #[test]
    fn bubblewrap_wraps_the_shell_command() {
        let (_dir, ctx) = ctx();
        let setup = bubblewrap_setup(false);
        let argv = ["-c".to_owned(), "echo hi".to_owned()];
        let launch = launch(Some(&setup), &ctx, false, "/bin/bash", &argv).unwrap();
        assert_eq!(launch.program, "bwrap");
        assert!(launch.args.iter().any(|a| a == "--unshare-net"));
        assert!(launch.args.ends_with(&["--".to_owned(), "/bin/bash".to_owned(), "-c".to_owned(), "echo hi".to_owned()]));
        assert!(!launch.job);
    }

    #[test]
    fn off_mode_and_unsandboxed_calls_leave_the_command_untouched() {
        let (_dir, ctx) = ctx();
        let argv = ["-c".to_owned(), "echo hi".to_owned()];
        let mut setup = bubblewrap_setup(false);
        setup.config.mode = SandboxMode::Off;
        let off = launch(Some(&setup), &ctx, false, "/bin/bash", &argv).unwrap();
        assert_eq!((off.program.as_str(), off.args.as_slice()), ("/bin/bash", &argv[..]));

        let setup = bubblewrap_setup(false);
        let bypass = launch(Some(&setup), &ctx, true, "/bin/bash", &argv).unwrap();
        assert_eq!((bypass.program.as_str(), bypass.args.as_slice()), ("/bin/bash", &argv[..]));

        let none = launch(None, &ctx, false, "/bin/bash", &argv).unwrap();
        assert_eq!(none.program, "/bin/bash");
    }

    #[test]
    fn landlock_restores_protected_files_changed_by_the_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path();
        std::fs::create_dir_all(ws.join(".git/hooks")).expect("mkdir");
        std::fs::write(ws.join(".git/hooks/pre-commit"), "orig").expect("write");
        std::fs::write(ws.join(".mcp.json"), "{}").expect("write");
        let mut setup = bubblewrap_setup(false);
        setup.detection.backend = Backend::Landlock;
        let ctx = ToolContext::for_tests(ws);
        let launch = launch(Some(&setup), &ctx, false, "/bin/sh", &[]).expect("launch");
        let protected = launch.protect();

        // What a sandboxed command could do: plant a hook, edit an MCP config, add a file, delete one.
        std::fs::write(ws.join(".git/hooks/pre-commit"), "evil").expect("write");
        std::fs::write(ws.join(".git/hooks/post-merge"), "evil").expect("write");
        std::fs::write(ws.join(".mcp.json"), "{\"x\":1}").expect("write");
        std::fs::remove_file(ws.join(".mcp.json")).expect("remove");
        std::fs::write(ws.join(".mcp.json"), "{\"x\":2}").expect("write");

        let settled = settle(protected).expect("something was restored");
        assert!(!settled.restored.is_empty());
        assert!(settled.message.starts_with("[sandbox] restored protected paths changed by the command: "), "{}", settled.message);
        assert_eq!(std::fs::read_to_string(ws.join(".git/hooks/pre-commit")).expect("read"), "orig");
        assert!(!ws.join(".git/hooks/post-merge").exists());
        assert_eq!(std::fs::read_to_string(ws.join(".mcp.json")).expect("read"), "{}");
    }

    #[cfg(unix)]
    #[test]
    fn planted_symlinks_are_removed_and_never_followed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        let ws = dir.path();
        std::fs::create_dir_all(ws.join(".claude")).expect("mkdir");
        std::fs::write(ws.join(".claude/settings.json"), "orig").expect("write");
        let target = outside.path().join("bashrc");
        std::fs::write(&target, "user file").expect("write");
        let mut setup = bubblewrap_setup(false);
        setup.detection.backend = Backend::Landlock;
        let ctx = ToolContext::for_tests(ws);
        let protected = launch(Some(&setup), &ctx, false, "/bin/sh", &[]).expect("launch").protect();

        // The command replaces a protected file by a link out of the workspace and plants a new one.
        std::fs::remove_file(ws.join(".claude/settings.json")).expect("remove");
        std::os::unix::fs::symlink(&target, ws.join(".claude/settings.json")).expect("symlink");
        std::os::unix::fs::symlink(&target, ws.join(".mcp.json")).expect("symlink");

        settle(protected).expect("something restored");
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "user file", "the link was followed");
        assert!(!std::fs::symlink_metadata(ws.join(".claude/settings.json")).expect("meta").file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(ws.join(".claude/settings.json")).expect("read"), "orig");
        assert!(std::fs::symlink_metadata(ws.join(".mcp.json")).is_err(), "planted link survived");
    }

    #[test]
    fn no_protected_snapshot_without_landlock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::for_tests(dir.path());
        // Seatbelt denies writes to protected paths itself, so nothing is snapshotted.
        let mut setup = bubblewrap_setup(false);
        setup.detection.backend = Backend::Seatbelt;
        let launch = launch(Some(&setup), &ctx, false, "/bin/sh", &[]).expect("launch");
        assert!(launch.protect().is_none());
        assert!(settle(None).is_none());
    }

    #[test]
    fn windows_job_wraps_through_the_helper_when_self_exe_is_known() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::for_tests(dir.path());
        let mut setup = bubblewrap_setup(false);
        setup.detection.backend = Backend::WindowsJob;
        setup.self_exe = PathBuf::from("/bin/crusty");
        let argv = ["-c".to_owned(), "echo hi".to_owned()];
        let l = launch(Some(&setup), &ctx, false, "/bin/bash", &argv).expect("launch");
        assert_eq!(l.program, "/bin/crusty");
        assert_eq!(&l.args[..3], &["__job-exec".to_owned(), "--".to_owned(), "/bin/bash".to_owned()]);
        assert!(!l.job, "the helper joins the job itself");

        setup.self_exe = PathBuf::new();
        let l = launch(Some(&setup), &ctx, false, "/bin/bash", &argv).expect("launch");
        assert_eq!(l.program, "/bin/bash");
        assert!(l.job, "without the helper the child joins the job after spawning");
    }

    #[test]
    fn unavailable_backend_fails_when_required() {
        let (_dir, ctx) = ctx();
        let mut setup = bubblewrap_setup(false);
        setup.detection = Detection {
            backend: Backend::None,
            available: false,
            confines_writes: false,
            blocks_network: false,
            detail: "no sandbox backend: bwrap is not on PATH".into(),
        };
        setup.config.fail_if_unavailable = true;
        let err = launch(Some(&setup), &ctx, false, "/bin/bash", &[]).expect_err("must refuse to run");
        assert!(matches!(&err, ToolError::Failed(m) if m.contains("bwrap is not on PATH")), "{err:?}");
    }
}
