//! Which shell runs a Bash tool command.

use std::path::{Path, PathBuf};

use crate::rtk_external::find_binary;

/// A shell program and the arguments placed before the command text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSpec {
    pub program: PathBuf,
    pub args_before_command: Vec<String>,
}

/// The shell to run commands with: `override_shell` (from `CRUSTY_SHELL`) when given; on
/// Unix `/bin/bash` or `/bin/sh`; on Windows `bash.exe` from `PATH` (Git Bash) or PowerShell.
pub fn detect_shell(override_shell: Option<&str>) -> ShellSpec {
    if let Some(shell) = override_shell.map(str::trim).filter(|s| !s.is_empty()) {
        return spec_for(PathBuf::from(shell));
    }
    if cfg!(windows) {
        match find_binary("bash") {
            Some(bash) => ShellSpec { program: bash, args_before_command: vec!["-c".into()] },
            None => ShellSpec {
                program: PathBuf::from("powershell.exe"),
                args_before_command: ["-NoProfile", "-NonInteractive", "-Command"].map(String::from).to_vec(),
            },
        }
    } else if Path::new("/bin/bash").is_file() {
        ShellSpec { program: PathBuf::from("/bin/bash"), args_before_command: vec!["-c".into()] }
    } else {
        ShellSpec { program: PathBuf::from("/bin/sh"), args_before_command: vec!["-c".into()] }
    }
}

fn spec_for(program: PathBuf) -> ShellSpec {
    let name = program.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let args = if name == "pwsh" || name == "powershell" {
        ["-NoProfile", "-NonInteractive", "-Command"].map(String::from).to_vec()
    } else {
        vec!["-c".into()]
    };
    ShellSpec { program, args_before_command: args }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins_and_picks_powershell_flags() {
        let spec = detect_shell(Some("C:/tools/pwsh.exe"));
        assert_eq!(spec.program, PathBuf::from("C:/tools/pwsh.exe"));
        assert_eq!(spec.args_before_command.last().map(String::as_str), Some("-Command"));
        assert_eq!(detect_shell(Some("/usr/bin/zsh")).args_before_command, vec!["-c".to_owned()]);
    }

    #[test]
    fn default_is_a_shell_that_exists_or_a_named_fallback() {
        let spec = detect_shell(None);
        assert!(!spec.args_before_command.is_empty());
        if cfg!(unix) {
            assert!(spec.program.is_file());
        }
    }
}
