//! Native port of the RTK idea: shrink command output noise while keeping every error.
//!
//! Every output first loses ANSI escapes, progress-bar rewrites and trailing blanks. A
//! command-specific filter (git, cargo, npm, test runners, listings, grep) then drops
//! noise, followed by generic duplicate collapsing and head/tail truncation. Lines that
//! look like errors survive every step when the command failed.

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use crusty_core::config::RtkConfig;
use crusty_core::filter::Filtered;
use regex::Regex;

// The patterns are literals, so a compile failure is a bug caught by the tests.
static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)").expect("valid ANSI pattern"));
static ERROR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)error|fatal").expect("valid error pattern"));
static IMPORTANT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)error|warning|fail|panic|exception|traceback|fatal").expect("valid important pattern"));
static NUMERIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"0x[0-9a-fA-F]+|\b[0-9a-fA-F]*\d[0-9a-fA-F]*\b").expect("valid numeric pattern"));
static GREP_MATCH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(.+?):(\d+):(.*)$").expect("valid grep pattern"));
static GREP_PLAIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(.+?):(.*)$").expect("valid grep plain pattern"));

const MAX_LINE_CHARS: usize = 300;
const LISTING_THRESHOLD: usize = 100;
const LISTING_HEAD: usize = 30;
const DIFF_HUNK_LINES: usize = 200;
/// `git log` options that print more than one line per commit; the one-line filter would lose them.
const LOG_DETAIL_FLAGS: [&str; 10] =
    ["-p", "--patch", "--stat", "--numstat", "--shortstat", "--name-only", "--name-status", "--raw", "--graph", "--oneline"];

/// Shrinks a shell command's output. `None` when nothing changes.
///
/// The transformer is `rtk:<command filter>` (or `rtk:clean`, `rtk:log-dedup`, `rtk:truncate`)
/// joined with `+` when several stages applied.
pub fn filter_command(command: &str, output: &str, exit_code: Option<i32>, cfg: &RtkConfig) -> Option<Filtered> {
    let strict = exit_code.is_some_and(|c| c != 0);
    let normalized = normalize(output);
    let mut names: Vec<&'static str> = Vec::new();

    let mut lines = match specific_filter(command, &normalized) {
        Some((name, filtered)) if filtered != normalized => {
            names.push(name);
            filtered
        }
        _ => normalized.clone(),
    };
    let before = lines.len();
    lines = dedup(lines);
    if lines.len() != before {
        names.push("rtk:log-dedup");
    }

    let (mut lines, truncated) = truncate(lines, cfg);
    if truncated {
        names.push("rtk:truncate");
    }
    if strict {
        // Last: every lossy step above may have dropped or merged an error line.
        lines = keep_errors(&normalized, lines);
    }

    let text = lines.join("\n");
    if text == output.trim_end_matches('\n') && names.is_empty() {
        return None;
    }
    if names.is_empty() {
        names.push("rtk:clean");
    }
    Some(Filtered { text, transformer: names.join("+") })
}

/// Removes ANSI escapes, keeps the last state of carriage-return progress lines, trims
/// trailing spaces and collapses runs of 3+ blank lines to one.
fn normalize(output: &str) -> Vec<String> {
    let stripped = ANSI.replace_all(output, "");
    let mut lines = Vec::new();
    let mut pending_blanks = 0usize;
    for raw in stripped.split('\n') {
        let line = raw.rsplit('\r').find(|s| !s.is_empty()).unwrap_or("").trim_end();
        if line.is_empty() {
            pending_blanks += 1;
            continue;
        }
        match pending_blanks {
            0 => {}
            1 | 2 => lines.extend(std::iter::repeat_n(String::new(), pending_blanks)),
            _ => lines.push(String::new()),
        }
        pending_blanks = 0;
        lines.push(line.to_owned());
    }
    lines
}

/// Picks the command-specific filter from the program name and its arguments.
fn specific_filter(command: &str, lines: &[String]) -> Option<(&'static str, Vec<String>)> {
    let words = primary_words(command);
    let prog = words.first().map_or("", |p| p.rsplit(['/', '\\']).next().unwrap_or(""));
    let prog = prog.strip_suffix(".exe").unwrap_or(prog);
    let sub = words.get(1).copied().unwrap_or("");
    match prog {
        "git" => match sub {
            "status" => Some(("rtk:git-status", git_status(lines))),
            "log" if !words.iter().any(|w| LOG_DETAIL_FLAGS.contains(w) || w.starts_with("--pretty") || w.starts_with("--format")) => {
                git_log(lines).map(|l| ("rtk:git-log", l))
            }
            "diff" | "show" => Some(("rtk:git-diff", git_diff(lines))),
            _ => None,
        },
        "cargo" | "rustc" => Some(("rtk:cargo", cargo(lines))),
        "npm" | "pnpm" | "yarn" | "bun" => match sub {
            "install" | "i" | "ci" | "add" | "run" | "test" | "t" | "build" | "exec" => Some(("rtk:npm", npm(lines))),
            _ => None,
        },
        "pytest" | "jest" | "vitest" => Some(("rtk:tests", tests(lines))),
        "go" | "dotnet" if sub == "test" => Some(("rtk:tests", tests(lines))),
        "mvn" | "mvnw" | "gradle" | "gradlew" if words.contains(&"test") => Some(("rtk:tests", tests(lines))),
        "python" | "python3" | "py" if words.windows(2).any(|w| w == ["-m", "pytest"]) => Some(("rtk:tests", tests(lines))),
        "ls" | "find" | "tree" | "dir" => listing(lines).map(|l| ("rtk:listing", l)),
        "grep" | "rg" => Some(("rtk:grep", grep(lines))),
        _ => None,
    }
}

/// Words of the last simple command that is not a `cd`, with leading `NAME=value`
/// assignments removed.
fn primary_words(command: &str) -> Vec<&str> {
    let segment = command
        .split([';', '&', '|', '\n'])
        .map(str::trim)
        .find(|s| !s.is_empty() && s.split_whitespace().next() != Some("cd"))
        .unwrap_or("");
    segment.split_whitespace().skip_while(|w| is_env_assignment(w)).collect()
}

fn is_env_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Restores error lines a lossy step dropped, so a failed command never loses them. Membership
/// is tested per trimmed line, not by substring.
fn keep_errors(input: &[String], mut out: Vec<String>) -> Vec<String> {
    let missing: Vec<&str> = {
        let present: HashSet<&str> = out.iter().map(|l| l.trim()).collect();
        let mut seen = HashSet::new();
        input.iter().map(|l| l.trim()).filter(|t| ERROR.is_match(t) && !present.contains(t) && seen.insert(*t)).collect()
    };
    if !missing.is_empty() {
        out.push("… error lines kept:".to_owned());
        out.extend(missing.into_iter().map(str::to_owned));
    }
    out
}

/// Collapses consecutive identical lines to `line  (×N)`, and runs of more than three lines
/// that differ only in numbers or hex digits to their first and last line. Error-looking
/// lines are never grouped.
fn dedup(lines: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if lines[i].is_empty() || IMPORTANT.is_match(&lines[i]) {
            out.push(lines[i].clone());
            i += 1;
            continue;
        }
        let key = NUMERIC.replace_all(&lines[i], "#");
        let mut j = i + 1;
        while j < lines.len() && !lines[j].is_empty() && !IMPORTANT.is_match(&lines[j]) && NUMERIC.replace_all(&lines[j], "#") == key {
            j += 1;
        }
        let group = &lines[i..j];
        let n = group.len();
        if n >= 2 && group.iter().all(|l| *l == group[0]) {
            out.push(format!("{}  (×{n})", group[0]));
        } else if n > 3 {
            out.push(group[0].clone());
            out.push(format!("  … {} similar lines omitted", n - 2));
            out.push(group[n - 1].clone());
        } else {
            out.extend_from_slice(group);
        }
        i = j;
    }
    out
}

/// Keeps the head and tail, plus every important line in between, once the output is longer
/// than `max_lines`.
fn truncate(lines: Vec<String>, cfg: &RtkConfig) -> (Vec<String>, bool) {
    let n = lines.len();
    if n <= cfg.max_lines {
        return (lines, false);
    }
    let head = cfg.head_lines.min(n);
    let tail_start = n.saturating_sub(cfg.tail_lines).max(head);
    let mut out = lines[..head].to_vec();
    let mut omitted = 0usize;
    let mut dropped = 0usize;
    for line in &lines[head..tail_start] {
        if IMPORTANT.is_match(line) {
            flush_omitted(&mut out, &mut omitted);
            out.push(line.clone());
        } else {
            omitted += 1;
            dropped += 1;
        }
    }
    flush_omitted(&mut out, &mut omitted);
    out.extend_from_slice(&lines[tail_start..]);
    (out, dropped > 0)
}

fn flush_omitted(out: &mut Vec<String>, omitted: &mut usize) {
    if *omitted > 0 {
        out.push(format!("… {omitted} lines omitted …"));
        *omitted = 0;
    }
}

/// Replaces a run of counted lines by one summary line.
fn collapse(out: &mut Vec<String>, count: &mut usize, what: &str) {
    if *count > 0 {
        out.push(format!("… {count} {what} collapsed"));
        *count = 0;
    }
}

fn cut(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_owned();
    }
    let mut s: String = line.chars().take(MAX_LINE_CHARS).collect();
    s.push('…');
    s
}

const GIT_SECTIONS: [&str; 5] = ["modified", "added", "deleted", "renamed", "untracked"];

/// `git status`: branch and header lines, then the changed files grouped by state.
fn git_status(lines: &[String]) -> Vec<String> {
    let mut header = Vec::new();
    let mut files: [Vec<String>; 5] = Default::default();
    let mut in_untracked = false;
    let mut in_changes = false;
    for line in lines {
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented {
            in_untracked = false;
            in_changes = false;
            if line.ends_with(':') {
                in_untracked = line.starts_with("Untracked files");
                in_changes = line.starts_with("Changes ") || line.starts_with("Unmerged");
            } else if !line.trim().is_empty() {
                header.push(strip_hint(line).to_owned());
            }
            continue;
        }
        let t = line.trim();
        if t.starts_with('(') {
            continue;
        }
        if in_untracked {
            files[4].push(t.to_owned());
        } else if in_changes {
            match parse_change(t) {
                Some((state, path)) => files[state].push(path.to_owned()),
                None => header.push(t.to_owned()),
            }
        } else {
            header.push(t.to_owned());
        }
    }
    let mut out = header;
    for (name, group) in GIT_SECTIONS.iter().zip(&files) {
        if group.is_empty() {
            continue;
        }
        out.push(format!("{name} ({}):", group.len()));
        out.extend(group.iter().map(|f| format!("  {f}")));
    }
    out
}

/// Index into `GIT_SECTIONS` and the path of a `git status` change line.
fn parse_change(t: &str) -> Option<(usize, &str)> {
    let (label, path) = t.split_once(':')?;
    let state = match label.trim() {
        "new file" | "copied" | "added" => 1,
        "modified" | "both modified" | "typechange" | "both added" => 0,
        "deleted" | "both deleted" => 2,
        "renamed" => 3,
        _ => return None,
    };
    Some((state, path.trim()))
}

/// Drops the parenthesised advice git appends to a summary line.
fn strip_hint(line: &str) -> &str {
    line.split(" (use ").next().unwrap_or(line)
}

/// `git log` (default format): one line per commit.
fn git_log(lines: &[String]) -> Option<Vec<String>> {
    struct Commit {
        hash: String,
        date: String,
        author: String,
        subject: String,
    }
    let mut commits: Vec<Commit> = Vec::new();
    for line in lines {
        if let Some(rest) = line.strip_prefix("commit ") {
            let hash: String = rest.split_whitespace().next().unwrap_or("").chars().take(7).collect();
            commits.push(Commit { hash, date: String::new(), author: String::new(), subject: String::new() });
        } else if let Some(c) = commits.last_mut() {
            if let Some(a) = line.strip_prefix("Author:") {
                c.author = a.split('<').next().unwrap_or("").trim().to_owned();
            } else if let Some(d) = line.strip_prefix("Date:") {
                c.date = d.trim().to_owned();
            } else if c.subject.is_empty() && line.starts_with("    ") && !line.trim().is_empty() {
                c.subject = line.trim().to_owned();
            }
        }
    }
    if commits.is_empty() {
        return None;
    }
    Some(commits.iter().map(|c| format!("{} {} {} {}", c.hash, c.date, c.author, c.subject)).collect())
}

/// `git diff` / `git show`: drops `index` lines and caps each file's hunks.
fn git_diff(lines: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut body = 0usize;
    let mut omitted = 0usize;
    let mut in_hunk = false;
    for line in lines {
        if line.starts_with("index ") {
            continue;
        }
        if line.starts_with("diff --git") {
            flush_omitted_diff(&mut out, &mut omitted);
            body = 0;
            in_hunk = false;
        } else if line.starts_with("@@") {
            in_hunk = true;
        }
        if in_hunk {
            body += 1;
            if body > DIFF_HUNK_LINES {
                omitted += 1;
                continue;
            }
        }
        out.push(line.clone());
    }
    flush_omitted_diff(&mut out, &mut omitted);
    out
}

fn flush_omitted_diff(out: &mut Vec<String>, omitted: &mut usize) {
    if *omitted > 0 {
        out.push(format!("… {omitted} more diff lines omitted for this file"));
        *omitted = 0;
    }
}

/// `cargo` / `rustc`: drops build progress, collapses passing tests into a count.
fn cargo(lines: &[String]) -> Vec<String> {
    const PROGRESS: [&str; 6] = ["Compiling ", "Checking ", "Downloaded ", "Downloading ", "Fresh ", "Blocking waiting"];
    let mut out = Vec::new();
    let mut passing = 0usize;
    for line in lines {
        let t = line.trim_start();
        if PROGRESS.iter().any(|p| t.starts_with(p)) {
            continue;
        }
        if t.starts_with("test ") && t.ends_with(" ... ok") {
            passing += 1;
            continue;
        }
        collapse(&mut out, &mut passing, "passing test lines");
        out.push(line.clone());
    }
    collapse(&mut out, &mut passing, "passing test lines");
    out
}

/// `npm` / `pnpm` / `yarn` / `bun`: drops progress, funding and audit boilerplate.
fn npm(lines: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut deprecated = 0usize;
    for line in lines {
        let t = line.trim();
        if t.starts_with("npm WARN deprecated") {
            deprecated += 1;
            continue;
        }
        let boilerplate = t.starts_with("npm http ")
            || t.contains("looking for funding")
            || t.contains("npm fund")
            || (t.contains("npm audit") && !t.starts_with("found "));
        if !boilerplate {
            out.push(line.clone());
        }
    }
    if deprecated > 0 {
        out.push(format!("npm WARN deprecated: {deprecated} warnings collapsed"));
    }
    out
}

/// Test runners: keeps failures, tracebacks and summaries; counts passing lines.
fn tests(lines: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut passed = 0usize;
    for line in lines {
        let t = line.trim();
        if is_passing_test(t) {
            passed += 1;
            continue;
        }
        collapse(&mut out, &mut passed, "passing test lines");
        out.push(line.clone());
    }
    collapse(&mut out, &mut passed, "passing test lines");
    out
}

fn is_passing_test(t: &str) -> bool {
    t.starts_with('✓')
        || t.starts_with('✔')
        || t.starts_with("--- PASS")
        || t.starts_with("=== RUN")
        || t.starts_with("PASS ")
        || t.starts_with("ok ")
        || t.starts_with("Passed ")
        || (t.contains(" PASSED") && !t.contains("FAILED"))
        || (!t.is_empty() && t.chars().all(|c| c == '.' || c == 's'))
}

/// `ls` / `find` / `tree` / `dir`: large listings are grouped by directory with counts.
fn listing(lines: &[String]) -> Option<Vec<String>> {
    let entries: Vec<&String> = lines.iter().filter(|l| !l.trim().is_empty() && !l.starts_with("total ")).collect();
    if entries.len() <= LISTING_THRESHOLD {
        return None;
    }
    let mut by_dir: BTreeMap<&str, usize> = BTreeMap::new();
    for entry in &entries {
        let token = entry.split_whitespace().last().unwrap_or("");
        let dir = token.rfind(['/', '\\']).map_or(".", |i| &token[..i]);
        *by_dir.entry(dir).or_insert(0) += 1;
    }
    let mut out: Vec<String> = lines.iter().filter(|l| l.starts_with("total ")).cloned().collect();
    out.extend(entries.iter().take(LISTING_HEAD).map(|e| (*e).clone()));
    out.push(format!("… {} more entries; by directory:", entries.len() - LISTING_HEAD));
    out.extend(by_dir.iter().map(|(dir, n)| format!("  {dir}: {n}")));
    Some(out)
}

/// `grep` / `rg`: matches grouped by file.
fn grep(lines: &[String]) -> Vec<String> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    let mut loose = Vec::new();
    for line in lines {
        let (path, entry) = if let Some(c) = GREP_MATCH.captures(line) {
            (c[1].to_owned(), format!("  {}: {}", &c[2], cut(&c[3])))
        } else if let Some(c) = GREP_PLAIN.captures(line).filter(|c| c[1].contains(['/', '\\', '.'])) {
            (c[1].to_owned(), format!("  {}", cut(&c[2])))
        } else {
            loose.push(cut(line));
            continue;
        };
        let slot = *index.entry(path.clone()).or_insert_with(|| {
            groups.push((path, Vec::new()));
            groups.len() - 1
        });
        groups[slot].1.push(entry);
    }
    let mut out = Vec::new();
    for (path, entries) in groups {
        out.push(format!("{path}:"));
        out.extend(entries);
    }
    out.extend(loose);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RtkConfig {
        RtkConfig::default()
    }

    fn run(command: &str, output: &str, exit: Option<i32>) -> Option<Filtered> {
        filter_command(command, output, exit, &cfg())
    }

    #[test]
    fn git_status_groups_by_state_and_drops_hints() {
        let out = "On branch main\nYour branch is up to date with 'origin/main'.\n\n\
Changes to be committed:\n  (use \"git restore --staged <file>...\" to unstage)\n\tnew file:   src/new.rs\n\n\
Changes not staged for commit:\n  (use \"git add <file>...\" to update what will be committed)\n\
\tmodified:   src/a.rs\n\tmodified:   src/b.rs\n\tdeleted:    old.txt\n\trenamed:    x.rs -> y.rs\n\n\
Untracked files:\n  (use \"git add <file>...\" to include in what will be committed)\n\ttodo.md\n\n\
no changes added to commit (use \"git add\" and/or \"git commit -a\")\n";
        let f = run("git status", out, Some(0)).unwrap();
        assert_eq!(f.transformer, "rtk:git-status");
        assert!(f.text.contains("modified (2):\n  src/a.rs\n  src/b.rs"));
        assert!(f.text.contains("added (1):\n  src/new.rs"));
        assert!(f.text.contains("deleted (1):"));
        assert!(f.text.contains("renamed (1):\n  x.rs -> y.rs"));
        assert!(f.text.contains("untracked (1):\n  todo.md"));
        assert!(!f.text.contains("(use"));
        assert!(f.text.starts_with("On branch main"));
        assert!(f.text.contains("no changes added to commit"));
    }

    #[test]
    fn git_log_one_line_per_commit() {
        let out = "commit 0123456789abcdef (HEAD -> main)\nAuthor: Ann Lee <ann@x.io>\nDate:   Mon Jan 1 10:00 2024\n\n    Fix parser\n\n    Longer body\n\n\
commit fedcba9876543210\nAuthor: Bob <bob@x.io>\nDate:   Tue Jan 2 11:00 2024\n\n    Add tests\n";
        let f = run("git log", out, Some(0)).unwrap();
        assert_eq!(f.transformer, "rtk:git-log");
        assert_eq!(f.text, "0123456 Mon Jan 1 10:00 2024 Ann Lee Fix parser\nfedcba9 Tue Jan 2 11:00 2024 Bob Add tests");
    }

    #[test]
    fn git_log_with_patch_stat_or_graph_keeps_its_detail() {
        let out = "commit 0123456789abcdef\nAuthor: Ann <a@x.io>\nDate:   Mon Jan 1\n\n    Fix\n\ndiff --git a/x b/x\n+added line\n";
        for cmd in ["git log -p", "git log --stat", "git log --graph --name-only"] {
            let text = run(cmd, out, Some(0)).map_or_else(
                || out.to_owned(),
                |f| {
                    assert_ne!(f.transformer, "rtk:git-log", "{cmd}");
                    f.text
                },
            );
            assert!(text.contains("+added line"), "{cmd}");
        }
    }

    #[test]
    fn git_log_oneline_is_left_alone() {
        assert!(run("git log --oneline", "abc1234 first\ndef5678 second\n", Some(0)).is_none());
    }

    #[test]
    fn git_diff_drops_index_and_caps_hunks() {
        let mut out = String::from("diff --git a/x.rs b/x.rs\nindex 111..222 100644\n--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n");
        for i in 0..300 {
            out.push_str(&format!("+line {i}\n"));
        }
        let f = run("git diff", &out, Some(0)).unwrap();
        assert!(f.transformer.starts_with("rtk:git-diff"));
        assert!(!f.text.contains("index 111"));
        assert!(f.text.contains("+line 0"));
        assert!(!f.text.contains("+line 299"));
        assert!(f.text.contains("more diff lines omitted"));
    }

    #[test]
    fn cargo_test_keeps_failure_and_summary_and_counts_passes() {
        let mut out = String::from("   Compiling demo v0.1.0\n    Finished test profile\n     Running unittests\nrunning 51 tests\n");
        for i in 0..50 {
            out.push_str(&format!("test ok_{i} ... ok\n"));
        }
        out.push_str("test broken ... FAILED\n\nfailures:\n\n---- broken stdout ----\nthread panicked at src/lib.rs:3\nassertion failed\n\nfailures:\n    broken\n\ntest result: FAILED. 50 passed; 1 failed; 0 ignored\n");
        let f = run("cargo test", &out, Some(101)).unwrap();
        assert!(f.text.contains("test broken ... FAILED"));
        assert!(f.text.contains("---- broken stdout ----"));
        assert!(f.text.contains("test result: FAILED. 50 passed; 1 failed"));
        assert!(f.text.contains("… 50 passing test lines collapsed"));
        assert!(!f.text.contains("Compiling demo"));
        assert!(!f.text.contains("test ok_3 ... ok"));
        assert!(f.transformer.starts_with("rtk:cargo"));
    }

    #[test]
    fn cargo_warning_block_is_kept_whole() {
        let out = "warning: unused variable: `x`\n  --> src/main.rs:2:9\n   |\n2  |     let x = 1;\n   |         ^ help: prefix\n";
        assert!(run("cargo check", out, Some(0)).is_none());
    }

    #[test]
    fn npm_install_noise_is_dropped() {
        let out = "npm WARN deprecated left-pad@1.0.0: use String.padStart\nnpm WARN deprecated foo@1.0.0: old\nnpm http fetch GET 200 https://registry\nadded 120 packages in 4s\n\n34 packages are looking for funding\n  run `npm fund` for details\n\nfound 0 vulnerabilities\n";
        let f = run("npm install", out, Some(0)).unwrap();
        assert_eq!(f.transformer, "rtk:npm");
        assert!(f.text.contains("added 120 packages in 4s"));
        assert!(f.text.contains("found 0 vulnerabilities"));
        assert!(f.text.contains("npm WARN deprecated: 2 warnings collapsed"));
        assert!(!f.text.contains("looking for funding"));
        assert!(!f.text.contains("npm http"));
    }

    #[test]
    fn pytest_failure_is_kept_and_passes_counted() {
        let mut out = String::new();
        for _ in 0..20 {
            out.push_str("tests/test_a.py::test_ok PASSED                                [ 5%]\n");
        }
        out.push_str("tests/test_a.py::test_bad FAILED                                 [100%]\n");
        out.push_str("def test_bad():\n>       assert 1 == 2\nE       AssertionError: assert 1 == 2\n");
        out.push_str("============ 1 failed, 20 passed in 0.31s ============\n");
        let f = run("pytest -v", &out, Some(1)).unwrap();
        assert!(f.transformer.starts_with("rtk:tests"));
        assert!(f.text.contains("test_bad FAILED"));
        assert!(f.text.contains("AssertionError: assert 1 == 2"));
        assert!(f.text.contains("1 failed, 20 passed"));
        assert!(f.text.contains("… 20 passing test lines collapsed"));
    }

    #[test]
    fn grep_groups_matches_by_file() {
        let out = "src/a.rs:10:fn alpha() {}\nsrc/a.rs:22:fn beta() {}\nsrc/b.rs:3:use alpha;\n";
        let f = run("grep -rn alpha src", out, Some(0)).unwrap();
        assert_eq!(f.transformer, "rtk:grep");
        assert_eq!(f.text, "src/a.rs:\n  10: fn alpha() {}\n  22: fn beta() {}\nsrc/b.rs:\n  3: use alpha;");
    }

    #[test]
    fn grep_cuts_long_lines() {
        let long = format!("a.rs:1:{}", "x".repeat(500));
        let f = run("rg x", &long, Some(0)).unwrap();
        assert!(f.text.lines().nth(1).unwrap().chars().count() <= 310);
    }

    #[test]
    fn listing_groups_large_directories() {
        let mut out = String::from("total 400\n");
        for i in 0..150 {
            out.push_str(&format!("src/mod{}/file{i}.rs\n", i % 3));
        }
        let f = run("find src", &out, Some(0)).unwrap();
        assert_eq!(f.transformer, "rtk:listing");
        assert!(f.text.contains("src/mod0: 50"));
        assert!(f.text.contains("120 more entries"));
        assert!(run("ls", "a\nb\n", Some(0)).is_none());
    }

    #[test]
    fn generic_dedup_counts_repeats() {
        let out = "start\nretry\nretry\nretry\nend\n";
        let f = run("make", out, Some(0)).unwrap();
        assert_eq!(f.transformer, "rtk:log-dedup");
        assert_eq!(f.text, "start\nretry  (×3)\nend");
    }

    #[test]
    fn failed_command_keeps_every_repeated_error_line() {
        let out: String = (0..6).map(|i| format!("error: cannot open f_{i}.o\n")).collect();
        let text = run("make", &out, Some(1)).map_or_else(|| out.clone(), |f| f.text);
        for i in 0..6 {
            assert!(text.contains(&format!("error: cannot open f_{i}.o")), "{text}");
        }
        assert!(!text.contains("similar lines omitted"), "{text}");
    }

    #[test]
    fn similar_lines_keep_first_and_last() {
        let out: String = (0..10).map(|i| format!("step {i} done\n")).collect();
        let f = run("make", &out, Some(0)).unwrap();
        assert_eq!(f.text, "step 0 done\n  … 8 similar lines omitted\nstep 9 done");
    }

    #[test]
    fn truncation_keeps_error_in_the_middle() {
        let mut out = String::new();
        for i in 0..1000 {
            if i == 500 {
                out.push_str("ERROR: disk full\n");
            } else {
                // Distinct shapes, so the dedup stage does not collapse them first.
                out.push_str(&format!("{}-{} step\n", "word".repeat(1 + i % 5), "tail".repeat(1 + i % 3)));
            }
        }
        let f = run("make build", &out, Some(0)).unwrap();
        assert!(f.transformer.ends_with("rtk:truncate"));
        assert!(f.text.contains("ERROR: disk full"));
        assert!(f.text.contains("lines omitted"));
        assert!(f.text.lines().count() < 400);
    }

    #[test]
    fn ansi_and_carriage_return_progress_are_removed() {
        let out = "\x1b[32mdone\x1b[0m\n10%\r50%\r100%\nfinal   \n";
        let f = run("make", out, Some(0)).unwrap();
        assert_eq!(f.text, "done\n100%\nfinal");
        assert_eq!(f.transformer, "rtk:clean");
    }

    #[test]
    fn none_when_nothing_changes() {
        assert!(run("make", "clean line\nanother\n", Some(0)).is_none());
        assert!(run("make", "", Some(0)).is_none());
    }

    #[test]
    fn non_zero_exit_keeps_error_lines_dropped_by_a_filter() {
        // The cargo filter drops "Compiling" lines; the error-looking one must survive a failure.
        let out = "   Compiling error-codes v0.1.0\n   Compiling demo v0.1.0\n";
        let f = run("cargo build", out, Some(1)).unwrap();
        assert!(f.text.contains("Compiling error-codes v0.1.0"));
        assert!(f.text.contains("error lines kept"));
        assert!(!f.text.contains("Compiling demo"));
        let ok = run("cargo build", out, Some(0)).unwrap();
        assert!(!ok.text.contains("error lines kept"));
    }

    #[test]
    fn env_prefix_and_cd_are_skipped() {
        let out = "On branch main\n\nnothing to commit, working tree clean\n";
        let clean = run("cd repo && FOO=1 git status", out, Some(0)).unwrap();
        assert!(clean.transformer.starts_with("rtk:git-status"));
        let staged = "Changes not staged for commit:\n\tmodified:   a.rs\n";
        let f = run("cd repo && FOO=1 git status", staged, Some(0)).unwrap();
        assert_eq!(f.transformer, "rtk:git-status");
    }
}
