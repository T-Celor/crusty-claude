//! Okapi BM25 ranking over identifier-aware tokens, used to rerank search fragments.

use std::collections::HashMap;

const K1: f64 = 1.2;
const B: f64 = 0.75;

/// Splits text into lowercase tokens. Punctuation and separators (`_`, `-`, `.`, `/`,
/// spaces) split; camelCase and PascalCase split (`HTTPRequest` gives `http`, `request`);
/// letter and digit runs split (`v2` gives `v`, `2`).
pub fn tokenize(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            push_token(&mut cur, &mut out);
            continue;
        }
        let boundary = i > 0 && !cur.is_empty() && {
            let prev = chars[i - 1];
            let next = chars.get(i + 1).copied();
            (prev.is_lowercase() && c.is_uppercase())
                || prev.is_alphabetic() != c.is_alphabetic()
                || (prev.is_uppercase() && c.is_uppercase() && next.is_some_and(char::is_lowercase))
        };
        if boundary {
            push_token(&mut cur, &mut out);
        }
        cur.push(c);
    }
    push_token(&mut cur, &mut out);
    out
}

fn push_token(cur: &mut String, out: &mut Vec<String>) {
    if !cur.is_empty() {
        out.push(cur.to_lowercase());
        cur.clear();
    }
}

/// BM25 score of each item for `query`, divided by the best score so that the scores lie
/// in 0..=1. All zeros when no item shares a token with the query.
pub fn bm25_scores(query: &str, items: &[String]) -> Vec<f64> {
    let n = items.len();
    let mut terms = tokenize(query);
    terms.sort();
    terms.dedup();
    if n == 0 || terms.is_empty() {
        return vec![0.0; n];
    }
    let docs: Vec<Vec<String>> = items.iter().map(|s| tokenize(s)).collect();
    let total: usize = docs.iter().map(Vec::len).sum();
    let avgdl = (total as f64 / n as f64).max(1.0);
    let tfs: Vec<HashMap<&str, usize>> = docs
        .iter()
        .map(|d| {
            let mut m = HashMap::new();
            for t in d {
                *m.entry(t.as_str()).or_insert(0) += 1;
            }
            m
        })
        .collect();

    let mut scores = vec![0.0_f64; n];
    for term in &terms {
        let df = tfs.iter().filter(|m| m.contains_key(term.as_str())).count();
        if df == 0 {
            continue;
        }
        let idf = ((n as f64 - df as f64 + 0.5) / (df as f64 + 0.5) + 1.0).ln();
        for (i, m) in tfs.iter().enumerate() {
            if let Some(&tf) = m.get(term.as_str()) {
                let tf = tf as f64;
                let dl = docs[i].len() as f64;
                scores[i] += idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * dl / avgdl));
            }
        }
    }
    let max = scores.iter().copied().fold(0.0_f64, f64::max);
    if max > 0.0 {
        for s in &mut scores {
            *s /= max;
        }
    }
    scores
}

/// Indices of the `k` best scores, best first. Ties keep their original order.
pub fn top_k(scores: &[f64], k: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..scores.len()).collect();
    idx.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    idx.truncate(k);
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_is_identifier_aware() {
        let cases: &[(&str, &[&str])] = &[
            ("parseHTTPRequest", &["parse", "http", "request"]),
            ("snake_case-kebab.dot/path", &["snake", "case", "kebab", "dot", "path"]),
            ("v2 release", &["v", "2", "release"]),
            ("FooBar2000", &["foo", "bar", "2000"]),
            ("  ", &[]),
            ("Écris le test", &["écris", "le", "test"]),
        ];
        for (input, expected) in cases {
            assert_eq!(tokenize(input), expected.to_vec(), "input {input:?}");
        }
    }

    #[test]
    fn bm25_orders_identifier_matches_first() {
        let items: Vec<String> = vec![
            "fn render_table(rows: &[Row])".into(),
            "struct ConfigLoader { path: PathBuf }".into(),
            "fn parse_config(path: &str) -> Config".into(),
        ];
        let scores = bm25_scores("parseConfig", &items);
        assert_eq!(scores.len(), 3);
        assert!((scores[2] - 1.0).abs() < 1e-12, "best item normalised to 1: {scores:?}");
        assert!(scores[1] > 0.0 && scores[1] < 1.0, "partial match: {scores:?}");
        assert_eq!(scores[0], 0.0);
        assert_eq!(top_k(&scores, 3), vec![2, 1, 0]);
    }

    #[test]
    fn bm25_handles_no_match_and_empty_inputs() {
        let items: Vec<String> = vec!["alpha beta".into(), "gamma".into()];
        assert_eq!(bm25_scores("zebra", &items), vec![0.0, 0.0]);
        assert_eq!(bm25_scores("", &items), vec![0.0, 0.0]);
        assert!(bm25_scores("x", &[]).is_empty());
    }

    #[test]
    fn top_k_is_stable_on_ties() {
        assert_eq!(top_k(&[0.5, 0.5, 0.9], 3), vec![2, 0, 1]);
        assert_eq!(top_k(&[1.0, 1.0], 1), vec![0]);
        assert_eq!(top_k(&[0.1, 0.2], 0), Vec::<usize>::new());
        assert_eq!(top_k(&[], 3), Vec::<usize>::new());
    }
}
