//! Build and test log triage: finds the error blocks of a failed command, keeps the best
//! ones with their context, and condenses the rest into omission markers.

use std::sync::LazyLock;

use crusty_core::config::TriageConfig;
use regex::Regex;

/// An error region of a log. Line numbers are 1-based and inclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorBlock {
    /// First line of the block (1-based).
    pub start_line: usize,
    /// Last line of the block (1-based, inclusive).
    pub end_line: usize,
    /// The block's lines joined with `\n`.
    pub text: String,
}

/// Error line patterns with their weight (0..=1).
static ERROR_PATTERNS: LazyLock<Vec<(Regex, f64)>> = LazyLock::new(|| {
    [
        (r"(?i)\berror\b", 1.0),
        (r"\bFAILED\b|\bFAIL:|\bfatal:|npm ERR!|\bpanicked at\b|\bTraceback\b|\w*Exception\b|\w+Error\b", 1.0),
        (r"(?i)\bassert", 0.6),
        (r"✕|×", 0.8),
        (r"[\w./\\-]+\.\w+:\d+(?::\d+)?", 0.6),
        (r"-->\s+\S+", 0.8),
    ]
    .into_iter()
    .map(|(p, w)| (Regex::new(p).expect("literal regex"), w))
    .collect()
});

/// Summary lines that are always kept at the end of a log.
static SUMMARY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"test result:|\b\d+ failed\b|error: could not compile|\bTests:|\b\d+ errors?\b").expect("literal regex"));

/// Largest weight among the error patterns that match `line`, 0.0 when none does.
fn line_weight(line: &str) -> f64 {
    ERROR_PATTERNS.iter().filter(|(re, _)| re.is_match(line)).map(|(_, w)| *w).fold(0.0, f64::max)
}

/// Extracts the error blocks of `text`.
///
/// Each error line gets `context_lines` of context on both sides; overlapping ranges merge.
/// At most `max_blocks` error blocks are kept, the highest-weighted first. Summary lines
/// (`test result:`, `N failed`, `error: could not compile`, `Tests:`) are kept in addition,
/// merged with the others when they touch. The result is ordered by line. The caller decides
/// whether to triage at all (`enabled`, `min_lines`).
pub fn extract(text: &str, cfg: &TriageConfig) -> Vec<ErrorBlock> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let last = lines.len() - 1;
    // (start, end, weight), 0-based inclusive, sorted by start.
    let mut ranges: Vec<(usize, usize, f64)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let w = line_weight(line);
        if w <= 0.0 {
            continue;
        }
        let start = i.saturating_sub(cfg.context_lines);
        let end = (i + cfg.context_lines).min(last);
        match ranges.last_mut() {
            Some(prev) if start <= prev.1 => {
                prev.1 = prev.1.max(end);
                prev.2 = prev.2.max(w);
            }
            _ => ranges.push((start, end, w)),
        }
    }

    ranges.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(&b.0)));
    ranges.truncate(cfg.max_blocks);
    for (i, line) in lines.iter().enumerate() {
        if SUMMARY.is_match(line) && !ranges.iter().any(|r| r.0 <= i && i <= r.1) {
            ranges.push((i, i, 0.0));
        }
    }

    ranges.sort_by_key(|r| r.0);
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e, _) in ranges {
        match merged.last_mut() {
            Some(prev) if s <= prev.1 => prev.1 = prev.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged.into_iter().map(|(s, e)| ErrorBlock { start_line: s + 1, end_line: e + 1, text: lines[s..=e].join("\n") }).collect()
}

/// The kept blocks in line order, with `[… N lines omitted …]` where lines were dropped.
/// With no blocks the whole log is one omission marker (or empty when the log is empty).
pub fn condense(text: &str, blocks: &[ErrorBlock]) -> String {
    let total = text.lines().count();
    let mut sorted: Vec<&ErrorBlock> = blocks.iter().collect();
    sorted.sort_by_key(|b| b.start_line);
    let mut out: Vec<String> = Vec::new();
    let mut covered = 0usize;
    for b in sorted {
        let gap = b.start_line.saturating_sub(covered + 1);
        if gap > 0 {
            out.push(marker(gap));
        }
        out.push(b.text.clone());
        covered = covered.max(b.end_line);
    }
    let tail = total.saturating_sub(covered);
    if tail > 0 {
        out.push(marker(tail));
    }
    out.join("\n")
}

fn marker(n: usize) -> String {
    format!("[… {n} lines omitted …]")
}

/// Usefulness of one error block for fixing a failure, 0..=1: the strongest matching
/// pattern, plus a little for each further error line and for a summary line. 0.0 when the
/// block has no error line.
pub fn block_score(text: &str) -> f64 {
    let weights: Vec<f64> = text.lines().map(line_weight).filter(|w| *w > 0.0).collect();
    let Some(max) = weights.iter().copied().reduce(f64::max) else {
        return 0.0;
    };
    let extra_lines = (weights.len() - 1).min(3) as f64 * 0.05;
    let summary = if text.lines().any(|l| SUMMARY.is_match(l)) { 0.1 } else { 0.0 };
    (max + extra_lines + summary).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(max_blocks: usize, context_lines: usize) -> TriageConfig {
        TriageConfig { enabled: true, min_lines: 0, max_blocks, context_lines }
    }

    const CARGO_FAILURE: &str = "\
   Compiling crusty-advisor v0.2.0
    Finished `test` profile [unoptimized + debuginfo] target(s) in 1.2s
     Running unittests src/lib.rs
running 2 tests
test parser::tests::parses_ok ... ok
test parser::tests::rejects_empty ... FAILED

failures:

---- parser::tests::rejects_empty stdout ----
thread 'parser::tests::rejects_empty' panicked at src/parser.rs:42:9:
assertion `left == right` failed
  left: 1
 right: 2

failures:
    parser::tests::rejects_empty

test result: FAILED. 1 passed; 1 failed; 0 finished in 0.01s

error: test failed, to report the bug
";

    const PYTHON_TRACEBACK: &str = "\
starting app
Traceback (most recent call last):
  File \"app.py\", line 10, in <module>
    main()
  File \"app.py\", line 7, in main
    total = 1 / 0
ZeroDivisionError: division by zero
";

    const NPM_ERROR: &str = "\
> app@1.0.0 test
> jest

npm ERR! code ELIFECYCLE
npm ERR! errno 1
npm ERR! app@1.0.0 test: `jest`
";

    #[test]
    fn cargo_failure_keeps_panic_and_summary() {
        let blocks = extract(CARGO_FAILURE, &cfg(8, 1));
        let joined: String = blocks.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("panicked at src/parser.rs:42:9"), "{joined}");
        assert!(joined.contains("test result: FAILED"), "{joined}");
        assert!(joined.contains("error: test failed"), "{joined}");
        assert!(!joined.contains("Compiling crusty-advisor"), "noise dropped: {joined}");
        let condensed = condense(CARGO_FAILURE, &blocks);
        assert!(condensed.contains("lines omitted"));
        assert!(condensed.lines().count() < CARGO_FAILURE.lines().count());
        assert!(blocks.windows(2).all(|w| w[0].end_line < w[1].start_line), "blocks ordered and disjoint");
    }

    #[test]
    fn python_traceback_and_npm_error_are_found() {
        let py = extract(PYTHON_TRACEBACK, &cfg(4, 0));
        assert!(py.iter().any(|b| b.text.contains("Traceback (most recent call last)")));
        assert!(py.iter().any(|b| b.text.contains("ZeroDivisionError")));
        let npm = extract(NPM_ERROR, &cfg(4, 0));
        assert!(npm.iter().any(|b| b.text.contains("npm ERR! code ELIFECYCLE")));
    }

    #[test]
    fn max_blocks_keeps_the_best() {
        let text = "warning: unused\nnote: x\nerror[E0308]: mismatched types\nnote: y\nnote: z\nnote: w\nnote: v\nassertion failed\n";
        let blocks = extract(text, &cfg(1, 0));
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].text.contains("error[E0308]"));
    }

    #[test]
    fn overlapping_context_merges() {
        let text = "a\nerror one\nb\nerror two\nc\nd\nd2\nd3\n";
        let blocks = extract(text, &cfg(8, 2));
        assert_eq!(blocks.len(), 1, "{blocks:?}");
        assert_eq!((blocks[0].start_line, blocks[0].end_line), (1, 6));
    }

    #[test]
    fn condense_marks_omissions() {
        let text = "1\n2\nerror\n4\n5\n6\nTests: 1 failed\n";
        let blocks = vec![
            ErrorBlock { start_line: 3, end_line: 3, text: "error".into() },
            ErrorBlock { start_line: 7, end_line: 7, text: "Tests: 1 failed".into() },
        ];
        assert_eq!(condense(text, &blocks), "[… 2 lines omitted …]\nerror\n[… 3 lines omitted …]\nTests: 1 failed");
        assert_eq!(condense("", &[]), "");
        assert_eq!(condense("a\nb", &[]), "[… 2 lines omitted …]");
    }

    #[test]
    fn no_error_no_blocks() {
        assert!(extract("all good\nfine\n", &cfg(8, 3)).is_empty());
        assert!(extract("", &cfg(8, 3)).is_empty());
    }

    #[test]
    fn block_scores_follow_patterns() {
        assert!(block_score("error[E0308]: mismatched types") > 0.9);
        assert!(block_score("assertion failed") < block_score("error: boom"));
        assert_eq!(block_score("note: nothing wrong"), 0.0);
        assert!(block_score("assertion a\nassertion b\nassertion c") > block_score("assertion a"));
        assert!(block_score("x") <= 1.0);
    }
}
