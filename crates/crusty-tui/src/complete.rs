//! File index and ranking for `@` completion.
//!
//! The index is built once in the background and refreshed on demand. Ranking combines a
//! subsequence match score, a bonus for short paths and a bonus for recently mentioned files.

use std::path::Path;

use ignore::WalkBuilder;

/// Most paths kept in the index.
pub const INDEX_CAP: usize = 20_000;

/// Walks `root`, honouring `.gitignore`, and returns relative paths with `/` separators.
pub fn build_index(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in WalkBuilder::new(root).build().flatten() {
        if out.len() >= INDEX_CAP {
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    out
}

/// Score of `query` as a subsequence of `path` (case-insensitive), or `None` when it is not
/// one. Consecutive matches and matches at word starts score higher.
pub fn fuzzy_score(query: &str, path: &str) -> Option<i64> {
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let mut score = 0i64;
    let mut qi = 0;
    let mut prev_match = false;
    let mut prev_char = '/';
    for pc in path.to_lowercase().chars() {
        if qi < q.len() && pc == q[qi] {
            score += 10;
            if prev_match {
                score += 6;
            }
            if matches!(prev_char, '/' | '_' | '-' | '.' | ' ') {
                score += 8;
            }
            qi += 1;
            prev_match = true;
        } else {
            prev_match = false;
        }
        prev_char = pc;
    }
    (qi == q.len()).then_some(score)
}

/// The best `limit` paths for `query`. `recent` lists recently mentioned paths, newest first.
pub fn rank_files(files: &[String], query: &str, recent: &[String], limit: usize) -> Vec<String> {
    let mut scored: Vec<(i64, &String)> = files
        .iter()
        .filter_map(|f| {
            let base = fuzzy_score(query, f)?;
            let short = (60 - f.len() as i64).max(0);
            let recency = recent.iter().position(|r| r == f).map_or(0, |i| (100 - 10 * i as i64).max(0));
            Some((base + short + recency, f))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().take(limit).map(|(_, f)| f.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn subsequence_required() {
        assert!(fuzzy_score("mr", "src/main.rs").is_some());
        assert!(fuzzy_score("zz", "src/main.rs").is_none());
    }

    #[test]
    fn word_start_and_consecutive_rank_higher() {
        let a = fuzzy_score("app", "src/app.rs").unwrap_or(0);
        let b = fuzzy_score("app", "src/happy_pad.rs").unwrap_or(0);
        assert!(a > b);
    }

    #[test]
    fn shorter_path_and_recent_win_ties() {
        let files = paths(&["a/long/name/mod.rs", "mod.rs", "other/mod.rs"]);
        let ranked = rank_files(&files, "mod", &[], 10);
        assert_eq!(ranked[0], "mod.rs");
        let recent = paths(&["other/mod.rs"]);
        let ranked = rank_files(&files, "mod", &recent, 10);
        assert_eq!(ranked[0], "other/mod.rs");
    }

    #[test]
    fn limit_is_applied() {
        let files = paths(&["a.rs", "b.rs", "c.rs"]);
        assert_eq!(rank_files(&files, "", &[], 2).len(), 2);
    }
}
