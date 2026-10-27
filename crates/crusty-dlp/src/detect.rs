//! Detectors for secrets and personal data, and the overlap resolution that turns their
//! candidates into the findings `scan` returns.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use crusty_core::config::DlpConfig;
use crusty_core::vault::RedactionKind;
use regex::Regex;

/// Confidence at or above which a finding is masked without asking the advisor.
pub(crate) const CERTAIN: f64 = 0.9;

/// One value to mask: its byte range in the scanned text, its kind and how sure the
/// detector is. Findings below [`CERTAIN`] are ambiguous.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// Byte offset where the value starts.
    pub start: usize,
    /// Byte offset just past the value.
    pub end: usize,
    /// Secret or personal-data kind, which also selects the placeholder tag.
    pub kind: RedactionKind,
    /// Detector confidence in `0..=1`.
    pub confidence: f64,
    /// Name of the rule that matched, for logs and tests.
    pub rule: &'static str,
}

/// A finding plus its tie-break priority (card and IBAN checksums beat the looser phone rule).
struct Candidate {
    finding: Finding,
    priority: u8,
}

fn candidate(start: usize, end: usize, kind: RedactionKind, confidence: f64, rule: &'static str, priority: u8) -> Candidate {
    Candidate { finding: Finding { start, end, kind, confidence, rule }, priority }
}

/// Known token formats: (rule name, pattern). Each match is masked whole.
const KNOWN_TOKENS: &[(&str, &str)] = &[
    ("anthropic_key", r"\bsk-ant-[A-Za-z0-9_-]{20,}"),
    ("openai_key", r"\bsk-(?:proj-)?[A-Za-z0-9_-]{20,}"),
    ("github_token", r"\bgh[pousr]_[A-Za-z0-9]{36}\b"),
    ("github_pat", r"\bgithub_pat_[A-Za-z0-9_]{22,}"),
    ("gitlab_pat", r"\bglpat-[A-Za-z0-9_-]{20,}"),
    ("slack_token", r"\bxox[abpr]-[A-Za-z0-9-]{10,}"),
    ("aws_access_key", r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b"),
    ("google_api_key", r"\bAIza[0-9A-Za-z_-]{35}"),
    ("stripe_key", r"\b(?:sk|rk|pk)_live_[A-Za-z0-9]{10,}"),
    ("sendgrid_key", r"\bSG\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}"),
    ("huggingface_token", r"\bhf_[A-Za-z0-9]{30,}"),
    ("npm_token", r"\bnpm_[A-Za-z0-9]{36}\b"),
    ("pypi_token", r"\bpypi-AgE[A-Za-z0-9_-]{50,}"),
    ("jwt", r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}"),
];

static KNOWN: LazyLock<Vec<(&'static str, Regex)>> =
    LazyLock::new(|| KNOWN_TOKENS.iter().map(|(name, pat)| (*name, Regex::new(pat).expect("literal regex"))).collect());
static PEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)").expect("literal regex")
});
static URL_PASSWORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Za-z][A-Za-z0-9+.-]*://[^\s:/@]+:([^\s/@]+)@").expect("literal regex"));
static AUTH_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\bauthorization["']?\s*[:=]\s*["']?(?:bearer|basic)\s+([A-Za-z0-9._~+/=-]{8,})"#).expect("literal regex")
});
static CONN_STRING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\b(?:password|pwd)\s*=\s*([^;\s"']{4,})"#).expect("literal regex"));
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)[A-Za-z0-9_-]*(?:api[_-]?key|secret|token|passw(?:or)?d|pwd|credential|private[_-]?key|access[_-]?key|client[_-]?secret)[A-Za-z0-9_-]*["']?[ \t]*[:=][ \t]*("[^"\r\n]*"|'[^'\r\n]*'|[^\s"'`,;]+)"#,
    )
    .expect("literal regex")
});
static ENTROPY_TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z0-9+/=_-]{24,}").expect("literal regex"));
static EMAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}").expect("literal regex"));
static PHONE_INTL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\+[0-9](?:[ .()\-]?[0-9]){7,14}").expect("literal regex"));
static PHONE_NATIONAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\([0-9]{3}\)[ .\-]?[0-9]{3}[ .\-][0-9]{4}").expect("literal regex"));
static PHONE_GROUPED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[0-9]{2,5}(?:[ .\-][0-9]{2,5}){2,4}\b").expect("literal regex"));
static CARD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[0-9](?:[ \-]?[0-9]){12,18}\b").expect("literal regex"));
static IBAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[A-Z]{2}[0-9]{2}(?: ?[A-Z0-9]){11,30}\b").expect("literal regex"));
static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x{27E6}[^\x{27E7}]*\x{27E7}").expect("literal regex"));

/// Finds the secrets and personal data in `text` that `cfg` asks to mask.
///
/// The result is sorted by position and non-overlapping. Values inside existing
/// `⟦...⟧` placeholders and values listed in `cfg.allow_values` are never reported.
/// Ambiguous findings (confidence below [`CERTAIN`]) are included so the caller can ask
/// an advisor about them.
pub fn scan(text: &str, cfg: &DlpConfig) -> Vec<Finding> {
    if !cfg.enabled {
        return Vec::new();
    }
    let mut cands = Vec::new();
    if cfg.secrets {
        detect_secrets(text, &mut cands);
    }
    if cfg.pii {
        detect_pii(text, &mut cands);
    }
    let placeholders: Vec<(usize, usize)> = PLACEHOLDER.find_iter(text).map(|m| (m.start(), m.end())).collect();
    cands.retain(|c| {
        let f = &c.finding;
        !placeholders.iter().any(|&(s, e)| f.start < e && s < f.end)
            && !cfg.allow_values.iter().any(|a| text.get(f.start..f.end) == Some(a.as_str()))
    });
    resolve(cands)
}

/// Keeps the best candidate of each overlapping group: most certain, then longest, then
/// highest priority, then leftmost.
fn resolve(mut cands: Vec<Candidate>) -> Vec<Finding> {
    cands.sort_by(|a, b| {
        b.finding
            .confidence
            .total_cmp(&a.finding.confidence)
            .then_with(|| (b.finding.end - b.finding.start).cmp(&(a.finding.end - a.finding.start)))
            .then_with(|| a.priority.cmp(&b.priority))
            .then_with(|| a.finding.start.cmp(&b.finding.start))
    });
    let mut kept: BTreeMap<usize, Finding> = BTreeMap::new();
    for c in cands {
        let f = c.finding;
        // Kept findings never overlap, so only the one starting closest before `f.end` can.
        let overlaps = kept.range(..f.end).next_back().is_some_and(|(_, k)| k.end > f.start);
        if !overlaps {
            kept.insert(f.start, f);
        }
    }
    kept.into_values().collect()
}

fn detect_secrets(text: &str, out: &mut Vec<Candidate>) {
    for (name, re) in KNOWN.iter() {
        for m in re.find_iter(text) {
            out.push(candidate(m.start(), m.end(), RedactionKind::Secret, 1.0, name, 0));
        }
    }
    for m in PEM.find_iter(text) {
        out.push(candidate(m.start(), m.end(), RedactionKind::Secret, 1.0, "pem_private_key", 0));
    }
    for (re, rule) in [(&*URL_PASSWORD, "url_password"), (&*AUTH_HEADER, "authorization_header"), (&*CONN_STRING, "connection_password")] {
        for c in re.captures_iter(text) {
            if let Some(g) = c.get(1).filter(|g| !is_non_secret(g.as_str())) {
                out.push(candidate(g.start(), g.end(), RedactionKind::Secret, 1.0, rule, 0));
            }
        }
    }
    for c in ASSIGNMENT.captures_iter(text) {
        let Some(g) = c.get(1) else { continue };
        let (s, e) = unquote(text, g.start(), g.end());
        let Some(v) = text.get(s..e) else { continue };
        if v.len() < 8 || is_non_secret(v) {
            continue;
        }
        let confidence = if high_entropy(v) { 0.95 } else { 0.6 };
        out.push(candidate(s, e, RedactionKind::Secret, confidence, "credential_assignment", 0));
    }
    for m in ENTROPY_TOKEN.find_iter(text) {
        let t = m.as_str();
        if t.len() > 2048 || is_hex(t) || is_uuid(t) || is_identifier(t) || path_like(text, m.start(), m.end(), t) {
            continue;
        }
        if char_classes(t) < 3 || shannon(t) < 4.0 {
            continue;
        }
        out.push(candidate(m.start(), m.end(), RedactionKind::Secret, 0.5, "high_entropy_token", 0));
    }
}

fn detect_pii(text: &str, out: &mut Vec<Candidate>) {
    for m in EMAIL.find_iter(text) {
        // `user:pass@host` is URL userinfo, which the URL-password rule handles.
        let in_userinfo = text.get(..m.start()).and_then(|s| s.chars().next_back()) == Some(':');
        if !in_userinfo && !allowed_email(m.as_str()) {
            out.push(candidate(m.start(), m.end(), RedactionKind::Email, 1.0, "email", 1));
        }
    }
    for m in PHONE_INTL.find_iter(text) {
        // A `+` glued to a word (`x+12345678`) is arithmetic, not a phone prefix.
        let glued = text.get(..m.start()).and_then(|s| s.chars().next_back()).is_some_and(char::is_alphanumeric);
        if !glued && (8..=15).contains(&digits(m.as_str()).len()) {
            out.push(candidate(m.start(), m.end(), RedactionKind::Phone, 1.0, "phone_international", 1));
        }
    }
    for m in PHONE_NATIONAL.find_iter(text) {
        out.push(candidate(m.start(), m.end(), RedactionKind::Phone, 1.0, "phone_national", 1));
    }
    for m in PHONE_GROUPED.find_iter(text) {
        let s = m.as_str();
        if (9..=15).contains(&digits(s).len()) && !is_ip_like(s) && !is_date_like(s) {
            out.push(candidate(m.start(), m.end(), RedactionKind::Phone, 1.0, "phone_grouped", 1));
        }
    }
    for m in CARD.find_iter(text) {
        if is_card(m.as_str()) {
            out.push(candidate(m.start(), m.end(), RedactionKind::Card, 1.0, "payment_card", 0));
        }
    }
    for m in IBAN.find_iter(text) {
        if iban_valid(m.as_str()) {
            out.push(candidate(m.start(), m.end(), RedactionKind::Iban, 1.0, "iban", 0));
        }
    }
}

/// Addresses that are examples, bots or SSH remotes rather than personal data.
fn allowed_email(email: &str) -> bool {
    let Some((local, domain)) = email.rsplit_once('@') else { return true };
    let local = local.to_ascii_lowercase();
    let domain = domain.to_ascii_lowercase();
    let reserved = ["example.com", "example.org", "example.net", "example", "localhost", "test", "invalid"];
    matches!(local.as_str(), "git" | "noreply" | "no-reply" | "donotreply")
        || domain.ends_with("noreply.github.com")
        || reserved.iter().any(|r| domain == *r || domain.ends_with(&format!(".{r}")))
}

fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// `a.b.c.d` with four groups of at most three digits is an IP address or a version.
fn is_ip_like(s: &str) -> bool {
    let groups: Vec<&str> = s.split('.').collect();
    s.contains('.') && groups.len() == 4 && groups.iter().all(|g| (1..=3).contains(&g.len()) && g.bytes().all(|b| b.is_ascii_digit()))
}

/// Dates such as `2026-10-27` start with a four-digit year.
fn is_date_like(s: &str) -> bool {
    s.split([' ', '.', '-']).next().is_some_and(|g| g.len() == 4)
}

fn is_card(s: &str) -> bool {
    let d = digits(s);
    (13..=19).contains(&d.len()) && d.starts_with(['2', '3', '4', '5', '6']) && luhn(&d)
}

fn luhn(d: &str) -> bool {
    let mut sum = 0;
    let mut double = false;
    for ch in d.chars().rev() {
        let Some(mut v) = ch.to_digit(10) else { return false };
        if double {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
        double = !double;
    }
    sum % 10 == 0
}

/// ISO 7064 mod-97 check of an IBAN, spaces allowed.
fn iban_valid(s: &str) -> bool {
    let chars: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !(15..=34).contains(&chars.len()) {
        return false;
    }
    let (head, tail) = chars.split_at(4);
    let mut rem: u32 = 0;
    for ch in tail.iter().chain(head.iter()) {
        if let Some(d) = ch.to_digit(10) {
            rem = (rem * 10 + d) % 97;
        } else if ch.is_ascii_uppercase() {
            rem = (rem * 100 + (*ch as u32 - 'A' as u32 + 10)) % 97;
        } else {
            return false;
        }
    }
    rem == 1
}

fn is_hex(t: &str) -> bool {
    t.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_uuid(t: &str) -> bool {
    t.len() == 36 && t.chars().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// Letters and underscores only: a camelCase or snake_case identifier, not a secret.
fn is_identifier(t: &str) -> bool {
    t.chars().all(|c| c.is_ascii_alphabetic() || c == '_' || c == '-')
}

/// Part of a path or URL: adjacent to a slash, or made of several slash-separated parts.
fn path_like(text: &str, start: usize, end: usize, token: &str) -> bool {
    let before = text.get(..start).and_then(|s| s.chars().next_back());
    let after = text.get(end..).and_then(|s| s.chars().next());
    matches!(before, Some('/' | '\\')) || matches!(after, Some('/' | '\\')) || token.matches('/').count() >= 2
}

/// Strips one pair of matching quotes from the span `s..e`.
fn unquote(text: &str, s: usize, e: usize) -> (usize, usize) {
    match text.get(s..e) {
        Some(inner) if inner.len() >= 2 && (inner.starts_with('"') || inner.starts_with('\'')) && inner.ends_with(&inner[..1]) => {
            (s + 1, e - 1)
        }
        _ => (s, e),
    }
}

/// Values that look like configuration references, placeholders or code, not secrets.
fn is_non_secret(v: &str) -> bool {
    let l = v.to_ascii_lowercase();
    v.contains(crusty_core::vault::OPEN)
        || v.starts_with(['$', '<', '{'])
        || v.contains(['(', ')'])
        || v.contains("://")
        || ["process.env", "os.environ", "getenv", "environ[", "env["].iter().any(|k| l.contains(k))
        || l.starts_with("xxx")
        || l.starts_with("your")
        || l.contains("example")
        || l.contains("placeholder")
        || v.chars().all(|c| v.starts_with(c))
        || v.chars().all(|c| matches!(c, 'x' | '*' | '.' | '-' | '_'))
        || matches!(l.as_str(), "changeme" | "change_me" | "null" | "none" | "nil" | "undefined" | "true" | "false" | "redacted")
}

fn high_entropy(v: &str) -> bool {
    shannon(v) >= 3.0 && char_classes(v) >= 3
}

/// Shannon entropy in bits per character.
fn shannon(s: &str) -> f64 {
    let mut counts: HashMap<char, usize> = HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0) += 1;
    }
    let n = s.chars().count() as f64;
    if n == 0.0 {
        return 0.0;
    }
    counts
        .values()
        .map(|&k| {
            let p = k as f64 / n;
            -p * p.log2()
        })
        .sum()
}

/// Number of character classes present: lowercase, uppercase, digits, other.
fn char_classes(s: &str) -> usize {
    let (mut lower, mut upper, mut digit, mut other) = (false, false, false, false);
    for c in s.chars() {
        if c.is_ascii_lowercase() {
            lower = true;
        } else if c.is_ascii_uppercase() {
            upper = true;
        } else if c.is_ascii_digit() {
            digit = true;
        } else {
            other = true;
        }
    }
    [lower, upper, digit, other].iter().filter(|&&b| b).count()
}
