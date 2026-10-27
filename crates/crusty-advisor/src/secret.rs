//! Heuristic probability that a string is a secret: entropy, length, character classes and
//! keyword context, lowered for the shapes that are rarely secrets (hex, UUIDs, paths,
//! dictionary words).

use std::collections::HashMap;
use std::sync::LazyLock;

use crusty_core::advisor::SecretCandidate;
use regex::Regex;

/// Well-known credential prefixes: almost certainly secrets.
static KNOWN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:sk-ant-|ghp_|gho_|ghs_|github_pat_|AKIA[0-9A-Z]{16}|xox[abprs]-|-----BEGIN )").expect("literal regex")
});

/// Canonical UUID text.
static UUID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$").expect("literal regex"));

/// Context words that make a nearby value more likely to be a secret.
const KEYWORDS: &[&str] = &["key", "token", "secret", "password", "passwd", "auth", "bearer"];

/// Probability, 0..=1, that `candidate` is a secret.
pub fn probability(candidate: &SecretCandidate) -> f64 {
    let value = candidate.value.trim();
    if value.chars().count() < 8 {
        return 0.0;
    }
    if KNOWN.is_match(value) {
        return 0.95;
    }
    if UUID.is_match(value) {
        return 0.05;
    }
    let len = value.chars().count() as f64;
    let entropy_term = (shannon_entropy(value) / 4.0).min(1.0);
    let length_term = (len / 20.0).min(1.0);
    let classes_term = char_classes(value) as f64 / 4.0;
    let mut p = 0.5 * entropy_term + 0.25 * length_term + 0.25 * classes_term;

    let context = candidate.context.to_ascii_lowercase();
    if KEYWORDS.iter().any(|k| context.contains(k)) {
        p = 0.4 + 0.6 * p;
    }
    if is_hex(value) {
        p *= 0.5;
    }
    if is_path_like(value) {
        p *= 0.3;
    }
    if is_dictionary_like(value) {
        p *= 0.3;
    }
    p.clamp(0.0, 1.0)
}

/// Shannon entropy of the characters, in bits per character.
fn shannon_entropy(s: &str) -> f64 {
    let mut counts: HashMap<char, usize> = HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0) += 1;
    }
    let n = s.chars().count() as f64;
    counts
        .values()
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

/// How many of the four classes (lower, upper, digit, symbol) occur.
fn char_classes(s: &str) -> usize {
    let has = |f: fn(&char) -> bool| s.chars().any(|c| f(&c));
    [has(|c| c.is_ascii_lowercase()), has(|c| c.is_ascii_uppercase()), has(|c| c.is_ascii_digit()), has(|c| !c.is_ascii_alphanumeric())]
        .into_iter()
        .filter(|&b| b)
        .count()
}

fn is_hex(s: &str) -> bool {
    s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_path_like(s: &str) -> bool {
    s.starts_with('/') || s.starts_with("~/") || s.starts_with("./") || s.starts_with("../") || s.contains('\\') || s.contains("://")
}

/// Lowercase words joined by separators (`authorization_header`), or one short lowercase word.
fn is_dictionary_like(s: &str) -> bool {
    let parts: Vec<&str> = s.split(['_', '-', '.', ' ']).filter(|p| !p.is_empty()).collect();
    let word = |p: &str| p.chars().all(|c| c.is_ascii_lowercase());
    if parts.len() >= 2 {
        return parts.iter().all(|p| word(p));
    }
    word(s) && s.len() < 20
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(value: &str, context: &str) -> SecretCandidate {
        SecretCandidate { value: value.into(), context: context.into() }
    }

    #[test]
    fn probability_table() {
        // (value, context, expected side of 0.5)
        let cases: &[(&str, &str, bool)] = &[
            ("AKIAIOSFODNN7EXAMPLE", "aws_key = ", true),
            ("q8Zr2LmX9pKd4vTn7WcB", "", true),
            ("hunter2hunter2", "password: ", true),
            ("sk-ant-api03-abcdefghijklmn", "", true),
            ("a3f9c2d8e1b74f6a9c0d5e7b2f1a8c3d", "", false),
            ("123e4567-e89b-12d3-a456-426614174000", "token = ", false),
            ("/usr/local/bin/cargo", "path = ", false),
            ("authorization_header_value", "", false),
            ("abc", "token", false),
        ];
        for (value, context, high) in cases {
            let p = probability(&cand(value, context));
            assert!((0.0..=1.0).contains(&p), "{value}: {p}");
            assert_eq!(p >= 0.5, *high, "{value} with context {context:?}: {p}");
        }
    }

    #[test]
    fn known_prefixes_and_uuids_are_extremes() {
        assert!(probability(&cand("ghp_abcdefghijklmnopqrstuvwx", "")) > 0.9);
        assert!(probability(&cand("123e4567-e89b-12d3-a456-426614174000", "secret")) < 0.1);
    }

    #[test]
    fn keyword_context_raises_probability() {
        let v = "Zx9kLm2Qp7Rt4Wv8";
        assert!(probability(&cand(v, "api_secret = ")) > probability(&cand(v, "int x = ")));
    }

    #[test]
    fn entropy_of_known_strings() {
        assert!((shannon_entropy("aaaa") - 0.0).abs() < 1e-12);
        assert!((shannon_entropy("abcd") - 2.0).abs() < 1e-12);
    }
}
