//! Turns a command into the sandboxed command to spawn, per backend.

use std::path::Path;

use crusty_core::config::SandboxMode;

use crate::detection::{Backend, Detection};
use crate::{Policy, SandboxError};

/// A command ready to spawn, possibly wrapped in a sandbox launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wrapped {
    /// Executable to spawn.
    pub program: String,
    /// Arguments for `program`.
    pub args: Vec<String>,
    /// Whether the command runs under an OS confinement mechanism.
    pub sandboxed: bool,
    /// Why the command is not confined, or a limitation the user should know about.
    pub note: Option<String>,
}

/// Wraps `program args` according to `det` and `policy`. `self_exe` is this binary, used by
/// the Landlock backend to re-enter through `__sandbox-exec`.
///
/// Unavailable backends yield `Err` when `policy.fail_if_unavailable` is set; otherwise the
/// command runs unwrapped with a note.
pub fn wrap(policy: &Policy, det: &Detection, program: &str, args: &[String], self_exe: &Path) -> Result<Wrapped, SandboxError> {
    if policy.mode == SandboxMode::Off {
        return Ok(plain(program, args, None));
    }
    if !det.available || det.backend == Backend::None {
        if policy.fail_if_unavailable {
            return Err(SandboxError::Unavailable(det.detail.clone()));
        }
        let note = format!("sandbox unavailable: {}", det.detail);
        return Ok(plain(program, args, Some(note)));
    }
    match det.backend {
        Backend::Bubblewrap => bubblewrap(policy, program, args),
        Backend::Landlock => landlock(policy, program, args, self_exe),
        Backend::Seatbelt => seatbelt(policy, program, args),
        Backend::WindowsJob => windows_job(program, args, self_exe, &det.detail),
        Backend::None => Ok(plain(program, args, Some(det.detail.clone()))),
    }
}

/// Runs the command through this binary's `__job-exec` helper, which joins a kill-on-close job
/// before it starts the command, so no grandchild can start outside the job. Without a known
/// executable the command runs unwrapped and the caller adopts it into a job after spawning.
fn windows_job(program: &str, args: &[String], self_exe: &Path, detail: &str) -> Result<Wrapped, SandboxError> {
    if self_exe.as_os_str().is_empty() {
        return Ok(plain(program, args, Some(detail.to_string())));
    }
    let mut argv: Vec<String> = ["__job-exec", "--", program].map(String::from).to_vec();
    argv.extend(args.iter().cloned());
    Ok(Wrapped { program: text(self_exe)?.to_string(), args: argv, sandboxed: true, note: None })
}

fn plain(program: &str, args: &[String], note: Option<String>) -> Wrapped {
    Wrapped { program: program.to_string(), args: args.to_vec(), sandboxed: false, note }
}

/// Rejects paths that would break a profile or argument list: non-UTF-8, NUL or line breaks.
fn text(path: &Path) -> Result<&str, SandboxError> {
    match path.to_str() {
        Some(s) if !s.contains(['\0', '\n', '\r']) => Ok(s),
        _ => Err(SandboxError::InvalidPath(path.to_path_buf())),
    }
}

fn bubblewrap(policy: &Policy, program: &str, args: &[String]) -> Result<Wrapped, SandboxError> {
    let mut argv: Vec<String> = ["--die-with-parent", "--new-session", "--unshare-pid"].map(String::from).to_vec();
    if !policy.network {
        argv.push("--unshare-net".to_string());
    }
    argv.extend(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp"].map(String::from));
    for dir in policy.writable_dirs() {
        let s = text(dir)?;
        argv.extend(["--bind", s, s].map(String::from));
    }
    // Bound after the writable binds so they stay read-only inside them. Missing paths are
    // skipped: bwrap cannot bind what does not exist.
    for protected in policy.protected.iter().filter(|p| p.exists()) {
        let s = text(protected)?;
        argv.extend(["--ro-bind", s, s].map(String::from));
    }
    argv.extend(["--chdir", text(&policy.cwd)?, "--", program].map(String::from));
    argv.extend(args.iter().cloned());
    Ok(Wrapped { program: "bwrap".to_string(), args: argv, sandboxed: true, note: None })
}

fn landlock(policy: &Policy, program: &str, args: &[String], self_exe: &Path) -> Result<Wrapped, SandboxError> {
    let json = serde_json::to_string(policy).map_err(|e| SandboxError::Unavailable(e.to_string()))?;
    let mut argv: Vec<String> = ["__sandbox-exec", "--policy", json.as_str(), "--", program].map(String::from).to_vec();
    argv.extend(args.iter().cloned());
    let note = policy
        .protected_in_writable()
        .map(|p| format!("landlock cannot keep {} read-only inside a writable root; it stays writable", p.display()));
    Ok(Wrapped { program: text(self_exe)?.to_string(), args: argv, sandboxed: true, note })
}

fn seatbelt(policy: &Policy, program: &str, args: &[String]) -> Result<Wrapped, SandboxError> {
    let profile = seatbelt_profile(policy)?;
    let mut argv = vec!["-p".to_string(), profile, program.to_string()];
    argv.extend(args.iter().cloned());
    Ok(Wrapped { program: "/usr/bin/sandbox-exec".to_string(), args: argv, sandboxed: true, note: None })
}

/// Builds the SBPL profile. SBPL applies the last matching rule, so the protected denies come
/// after the allows.
fn seatbelt_profile(policy: &Policy) -> Result<String, SandboxError> {
    let mut sbpl = String::from(
        "(version 1)(allow default)(deny file-write*)\
         (allow file-write* (subpath \"/private/tmp\") (subpath \"/private/var/folders\") \
         (literal \"/dev/null\") (literal \"/dev/tty\") (regex #\"^/dev/fd/\")",
    );
    for dir in policy.writable_dirs() {
        sbpl.push_str(&format!(" (subpath \"{}\")", sbpl_escape(text(dir)?)));
    }
    sbpl.push(')');
    if !policy.protected.is_empty() {
        sbpl.push_str("(deny file-write*");
        for p in &policy.protected {
            sbpl.push_str(&format!(" (subpath \"{}\")", sbpl_escape(text(p)?)));
        }
        sbpl.push(')');
    }
    if !policy.network {
        sbpl.push_str("(deny network-outbound)(allow network-outbound (remote unix-socket))");
    }
    Ok(sbpl)
}

/// Escapes a string for an SBPL double-quoted literal.
fn sbpl_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn policy(ws: &Path, mode: SandboxMode, network: bool) -> Policy {
        Policy {
            mode,
            workspace: ws.to_path_buf(),
            cwd: ws.to_path_buf(),
            writable_roots: Vec::new(),
            network,
            protected: Vec::new(),
            fail_if_unavailable: false,
        }
    }

    fn detection(backend: Backend, available: bool) -> Detection {
        Detection { backend, available, confines_writes: true, blocks_network: true, detail: "test backend".to_string() }
    }

    #[test]
    fn bubblewrap_workspace_write_without_network() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().to_path_buf();
        std::fs::create_dir_all(ws.join(".git/hooks")).expect("mkdir");
        let mut p = policy(&ws, SandboxMode::WorkspaceWrite, false);
        // One existing (bound read-only) and one missing (skipped).
        p.protected = vec![ws.join(".git/hooks"), ws.join(".claude")];

        let w = wrap(&p, &detection(Backend::Bubblewrap, true), "ls", &strs(&["-la"]), Path::new("/x")).expect("wrap");
        let ws_s = ws.to_str().expect("utf8").to_string();
        let hooks = ws.join(".git/hooks").to_str().expect("utf8").to_string();
        let expected = strs(&[
            "--die-with-parent",
            "--new-session",
            "--unshare-pid",
            "--unshare-net",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--bind",
            &ws_s,
            &ws_s,
            "--ro-bind",
            &hooks,
            &hooks,
            "--chdir",
            &ws_s,
            "--",
            "ls",
            "-la",
        ]);
        assert_eq!(w.program, "bwrap");
        assert_eq!(w.args, expected);
        assert!(w.sandboxed && w.note.is_none());
    }

    #[test]
    fn bubblewrap_readonly_binds_nothing_writable_and_keeps_network() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path();
        let w = wrap(&policy(ws, SandboxMode::ReadOnly, true), &detection(Backend::Bubblewrap, true), "ls", &[], Path::new("/x"))
            .expect("wrap");
        assert!(!w.args.iter().any(|a| a == "--bind" || a == "--unshare-net"));
        assert_eq!(w.args.last().map(String::as_str), Some("ls"));
    }

    #[test]
    fn seatbelt_profile_text_and_escaping() {
        let mut p = policy(Path::new("/work/ws"), SandboxMode::WorkspaceWrite, false);
        p.writable_roots = vec![PathBuf::from("/cache")];
        p.protected = vec![PathBuf::from("/work/ws/.git/hooks")];
        let profile = seatbelt_profile(&p).expect("profile");
        assert_eq!(
            profile,
            "(version 1)(allow default)(deny file-write*)(allow file-write* \
             (subpath \"/private/tmp\") (subpath \"/private/var/folders\") (literal \"/dev/null\") \
             (literal \"/dev/tty\") (regex #\"^/dev/fd/\") (subpath \"/work/ws\") (subpath \"/cache\"))\
             (deny file-write* (subpath \"/work/ws/.git/hooks\"))\
             (deny network-outbound)(allow network-outbound (remote unix-socket))"
        );

        p.workspace = PathBuf::from("/a\"b\\c");
        p.writable_roots.clear();
        p.protected.clear();
        p.network = true;
        let profile = seatbelt_profile(&p).expect("profile");
        assert!(profile.contains("(subpath \"/a\\\"b\\\\c\")"));
        assert!(!profile.contains("network-outbound"));
    }

    #[test]
    fn seatbelt_readonly_allows_only_temp() {
        let p = policy(Path::new("/ws"), SandboxMode::ReadOnly, true);
        let profile = seatbelt_profile(&p).expect("profile");
        assert!(!profile.contains("/ws"));
        assert!(!profile.contains("(deny file-write* ("));
    }

    #[test]
    fn seatbelt_refuses_newline_in_path() {
        let mut p = policy(Path::new("/ws"), SandboxMode::WorkspaceWrite, true);
        p.writable_roots = vec![PathBuf::from("/x\ny")];
        let err = wrap(&p, &detection(Backend::Seatbelt, true), "ls", &[], Path::new("/x")).expect_err("newline must be refused");
        assert!(matches!(err, SandboxError::InvalidPath(_)));
    }

    #[test]
    fn landlock_wraps_through_self_exe_with_policy_json() {
        let p = policy(Path::new("/ws"), SandboxMode::WorkspaceWrite, true);
        let w = wrap(&p, &detection(Backend::Landlock, true), "ls", &strs(&["-a"]), Path::new("/bin/crusty")).expect("wrap");
        assert_eq!(w.program, "/bin/crusty");
        assert_eq!(&w.args[..4], &strs(&["__sandbox-exec", "--policy", &serde_json::to_string(&p).expect("json"), "--"])[..]);
        assert_eq!(&w.args[4..], &strs(&["ls", "-a"])[..]);
    }

    #[test]
    fn landlock_notes_protected_path_inside_writable_root() {
        let mut p = policy(Path::new("/ws"), SandboxMode::WorkspaceWrite, true);
        p.protected = vec![PathBuf::from("/ws/.git/hooks")];
        let w = wrap(&p, &detection(Backend::Landlock, true), "ls", &[], Path::new("/bin/crusty")).expect("wrap");
        assert!(w.note.is_some_and(|n| n.contains(".git/hooks")));
    }

    #[test]
    fn windows_job_runs_through_the_helper() {
        let p = policy(Path::new("/ws"), SandboxMode::ReadOnly, false);
        let det = detection(Backend::WindowsJob, true);
        let w = wrap(&p, &det, "cmd", &strs(&["/C", "dir"]), Path::new("C:/bin/crusty.exe")).expect("wrap");
        assert_eq!(w.program, "C:/bin/crusty.exe");
        assert_eq!(w.args, strs(&["__job-exec", "--", "cmd", "/C", "dir"]));
        assert!(w.sandboxed);

        // Unknown executable: run as before, the caller adopts the child into a job.
        let w = wrap(&p, &det, "cmd", &strs(&["/C"]), Path::new("")).expect("wrap");
        assert_eq!(w.program, "cmd");
        assert!(!w.sandboxed && w.note.is_some());
    }

    #[test]
    fn unavailable_fails_or_runs_unwrapped_with_note() {
        let det = detection(Backend::None, false);
        let mut p = policy(Path::new("/ws"), SandboxMode::WorkspaceWrite, true);

        p.fail_if_unavailable = true;
        let err = wrap(&p, &det, "ls", &[], Path::new("/x")).expect_err("must fail");
        assert_eq!(err, SandboxError::Unavailable("test backend".to_string()));

        p.fail_if_unavailable = false;
        let w = wrap(&p, &det, "ls", &strs(&["-a"]), Path::new("/x")).expect("wrap");
        assert!(!w.sandboxed);
        assert_eq!((w.program.as_str(), w.args.as_slice()), ("ls", &strs(&["-a"])[..]));
        assert_eq!(w.note.as_deref(), Some("sandbox unavailable: test backend"));
    }

    #[test]
    fn off_mode_never_wraps_or_fails() {
        let mut p = policy(Path::new("/ws"), SandboxMode::Off, true);
        p.fail_if_unavailable = true;
        let w = wrap(&p, &detection(Backend::Bubblewrap, true), "ls", &[], Path::new("/x")).expect("off mode must not fail");
        assert!(!w.sandboxed && w.note.is_none() && w.program == "ls");
    }
}
