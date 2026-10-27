//! Environment handed to shell commands: secrets removed by name, so commands never see API keys.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// Variables a command needs to run; kept even when a pattern matches them (e.g. `*_HOME`).
const ALLOWED: &[&str] = &[
    "PATH",
    "PATHEXT",
    "HOME",
    "USERPROFILE",
    "USER",
    "USERNAME",
    "LOGNAME",
    "SHELL",
    "TERM",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMP",
    "TEMP",
    "TMPDIR",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMDATA",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "GOPATH",
    "JAVA_HOME",
];

/// Keeps the variables whose names match none of `patterns` (case-insensitive globs such as
/// `*_API_KEY`), plus the `ALLOWED` names. If any pattern fails to compile, only the `ALLOWED`
/// names are kept.
pub fn scrubbed_env(vars: impl Iterator<Item = (String, String)>, patterns: &[String]) -> Vec<(String, String)> {
    let Some(secrets) = compile(patterns) else {
        return vars.filter(|(name, _)| is_allowed(name)).collect();
    };
    vars.filter(|(name, _)| is_allowed(name) || !secrets.is_match(name)).collect()
}

fn is_allowed(name: &str) -> bool {
    ALLOWED.iter().any(|allowed| allowed.eq_ignore_ascii_case(name))
}

/// The compiled patterns, or `None` when any pattern is invalid (the caller then fails closed).
fn compile(patterns: &[String]) -> Option<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(GlobBuilder::new(pattern).case_insensitive(true).build().ok()?);
    }
    builder.build().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(names: &[&str]) -> Vec<(String, String)> {
        names.iter().map(|n| (n.to_string(), format!("value-of-{n}"))).collect()
    }

    #[test]
    fn removes_secret_names_and_keeps_the_rest() {
        let patterns: Vec<String> = ["*_API_KEY", "*_TOKEN", "GITHUB_TOKEN"].map(String::from).to_vec();
        let kept = scrubbed_env(
            pairs(&["PATH", "HOME", "ANTHROPIC_API_KEY", "GITHUB_TOKEN", "my_token", "anthropic_api_key"]).into_iter(),
            &patterns,
        );
        let names: Vec<&str> = kept.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["PATH", "HOME"]);
    }

    #[test]
    fn no_patterns_keeps_everything() {
        let kept = scrubbed_env(pairs(&["ANTHROPIC_API_KEY"]).into_iter(), &[]);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn invalid_pattern_keeps_only_allowlisted_names() {
        let patterns: Vec<String> = ["*_API_KEY", "[unclosed"].map(String::from).to_vec();
        let kept = scrubbed_env(pairs(&["PATH", "HOME", "ANTHROPIC_API_KEY", "FOO"]).into_iter(), &patterns);
        let names: Vec<&str> = kept.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["PATH", "HOME"]);
    }

    #[test]
    fn allowlisted_names_survive_matching_patterns() {
        let patterns: Vec<String> = ["*_HOME", "*_TOKEN"].map(String::from).to_vec();
        let kept = scrubbed_env(pairs(&["JAVA_HOME", "CARGO_HOME", "GITHUB_TOKEN"]).into_iter(), &patterns);
        let names: Vec<&str> = kept.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["JAVA_HOME", "CARGO_HOME"]);
    }
}
