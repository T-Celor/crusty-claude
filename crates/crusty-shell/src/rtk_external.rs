//! Optional hand-off of supported commands to the external `rtk` binary, which filters their
//! output itself.

use std::path::{Path, PathBuf};

/// First words of commands the external `rtk` binary knows how to filter.
const RTK_COMMANDS: &[&str] = &[
    "git",
    "ls",
    "tree",
    "grep",
    "rg",
    "find",
    "cargo",
    "npm",
    "pnpm",
    "docker",
    "kubectl",
    "gh",
    "diff",
    "wc",
    "curl",
    "tsc",
    "jest",
    "vitest",
    "dotnet",
    "prisma",
    "next",
    "playwright",
    "bun",
    "pytest",
    "go",
];

/// `"<rtk_binary> <command>"` with the command text unchanged, when `command` is one simple
/// command that `rtk` supports (no pipes, lists, redirections, substitutions or newlines).
/// `None` otherwise.
pub fn rtk_rewrite(command: &str, rtk_binary: &str) -> Option<String> {
    if command.contains(['|', '&', ';', '<', '>', '`', '\n', '\r']) || command.contains("$(") {
        return None;
    }
    let first = command.split_whitespace().next()?;
    RTK_COMMANDS.contains(&first).then(|| format!("{rtk_binary} {command}"))
}

/// Finds an executable on `PATH`. A name with a directory part is checked as given. On
/// Windows, `.exe` is tried when the name has no extension.
pub fn find_binary(name: &str) -> Option<PathBuf> {
    if Path::new(name).components().count() > 1 {
        return Path::new(name).is_file().then(|| PathBuf::from(name));
    }
    let mut names = vec![name.to_owned()];
    if cfg!(windows) && Path::new(name).extension().is_none() {
        names.push(format!("{name}.exe"));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).flat_map(|dir| names.iter().map(move |n| dir.join(n))).find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_supported_simple_commands() {
        assert_eq!(rtk_rewrite("git status", "rtk").as_deref(), Some("rtk git status"));
        // The rewrite is exactly `<binary> <original command>`, with nothing trimmed or added.
        assert_eq!(rtk_rewrite("  cargo test  ", "rtk").as_deref(), Some("rtk   cargo test  "));
        assert_eq!(rtk_rewrite("cargo test", "rtk").as_deref(), Some("rtk cargo test"));
    }

    #[test]
    fn rejects_compound_or_unsupported_commands() {
        assert_eq!(rtk_rewrite("git status | head", "rtk"), None);
        assert_eq!(rtk_rewrite("git status && git diff", "rtk"), None);
        assert_eq!(rtk_rewrite("echo $(git status)", "rtk"), None);
        assert_eq!(rtk_rewrite("git log > out.txt", "rtk"), None);
        assert_eq!(rtk_rewrite("make", "rtk"), None);
        assert_eq!(rtk_rewrite("", "rtk"), None);
    }

    #[test]
    fn finds_a_binary_given_by_path() {
        let exe = std::env::current_exe().expect("test binary path");
        assert_eq!(find_binary(exe.to_str().expect("utf-8 path")), Some(exe));
        assert_eq!(find_binary("crusty-no-such-binary-xyz"), None);
    }
}
