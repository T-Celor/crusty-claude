//! Placeholders for masked values (data-loss prevention).
//!
//! The DLP stage (`crusty-dlp`) replaces secrets and personal data in outgoing requests
//! with placeholders such as `⟦SECRET_1⟧`. The same value always gets the same
//! placeholder within a vault, so a masked prompt prefix stays byte-identical across
//! requests and the provider cache keeps working. The engine restores real values in tool
//! inputs before a tool runs and in the text the user sees; the model never sees them.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Opening and closing marks of a placeholder.
pub const OPEN: char = '⟦';
pub const CLOSE: char = '⟧';

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionKind {
    Secret,
    Email,
    Phone,
    Card,
    Iban,
}

impl RedactionKind {
    pub fn tag(self) -> &'static str {
        match self {
            RedactionKind::Secret => "SECRET",
            RedactionKind::Email => "EMAIL",
            RedactionKind::Phone => "PHONE",
            RedactionKind::Card => "CARD",
            RedactionKind::Iban => "IBAN",
        }
    }

    pub fn is_secret(self) -> bool {
        self == RedactionKind::Secret
    }

    fn from_tag(tag: &str) -> Option<Self> {
        [RedactionKind::Secret, RedactionKind::Email, RedactionKind::Phone, RedactionKind::Card, RedactionKind::Iban]
            .into_iter()
            .find(|k| k.tag() == tag)
    }
}

#[derive(Debug, Default)]
struct Inner {
    by_value: HashMap<String, String>,
    by_placeholder: HashMap<String, String>,
    counters: HashMap<RedactionKind, u32>,
}

/// Value ↔ placeholder table of a session. Thread-safe; cheap to share in an `Arc`.
#[derive(Debug, Default)]
pub struct SecretVault {
    inner: Mutex<Inner>,
}

impl SecretVault {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The placeholder of `value`, created on first use (`⟦EMAIL_2⟧`).
    pub fn placeholder(&self, value: &str, kind: RedactionKind) -> String {
        let mut inner = self.lock();
        if let Some(p) = inner.by_value.get(value) {
            return p.clone();
        }
        let n = inner.counters.entry(kind).or_insert(0);
        *n += 1;
        let placeholder = format!("{OPEN}{}_{n}{CLOSE}", kind.tag());
        inner.by_value.insert(value.to_owned(), placeholder.clone());
        inner.by_placeholder.insert(placeholder.clone(), value.to_owned());
        placeholder
    }

    /// The real value behind a placeholder.
    pub fn value_of(&self, placeholder: &str) -> Option<String> {
        self.lock().by_placeholder.get(placeholder).cloned()
    }

    pub fn len(&self) -> usize {
        self.lock().by_value.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Replaces every known placeholder of `text` with its value. Unknown placeholders
    /// (made up by the model) are left as they are.
    pub fn rehydrate<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if !text.contains(OPEN) {
            return Cow::Borrowed(text);
        }
        let inner = self.lock();
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        let mut changed = false;
        while let Some(start) = rest.find(OPEN) {
            out.push_str(&rest[..start]);
            let after = &rest[start..];
            match placeholder_len(after).and_then(|len| inner.by_placeholder.get(&after[..len]).map(|v| (len, v))) {
                Some((len, value)) => {
                    out.push_str(value);
                    rest = &after[len..];
                    changed = true;
                }
                None => {
                    out.push(OPEN);
                    rest = &after[OPEN.len_utf8()..];
                }
            }
        }
        out.push_str(rest);
        if changed { Cow::Owned(out) } else { Cow::Borrowed(text) }
    }

    /// Rehydrates every string of a JSON value in place (keys included). Returns how many
    /// strings changed.
    pub fn rehydrate_json(&self, value: &mut Value) -> usize {
        match value {
            Value::String(s) => match self.rehydrate(s) {
                Cow::Owned(new) => {
                    *s = new;
                    1
                }
                Cow::Borrowed(_) => 0,
            },
            Value::Array(items) => items.iter_mut().map(|v| self.rehydrate_json(v)).sum(),
            Value::Object(map) => {
                let mut changed = 0;
                let keys: Vec<String> = map.keys().filter(|k| k.contains(OPEN)).cloned().collect();
                for key in keys {
                    if let Cow::Owned(new_key) = self.rehydrate(&key)
                        && let Some(v) = map.remove(&key)
                    {
                        map.insert(new_key, v);
                        changed += 1;
                    }
                }
                changed + map.values_mut().map(|v| self.rehydrate_json(v)).sum::<usize>()
            }
            _ => 0,
        }
    }
}

/// Length in bytes of a well-formed placeholder at the start of `s` (`⟦TAG_N⟧`).
fn placeholder_len(s: &str) -> Option<usize> {
    let body = s.strip_prefix(OPEN)?;
    let end = body.find(CLOSE)?;
    let (tag, n) = body[..end].rsplit_once('_')?;
    (RedactionKind::from_tag(tag).is_some() && !n.is_empty() && n.len() <= 9 && n.bytes().all(|b| b.is_ascii_digit()))
        .then(|| OPEN.len_utf8() + end + CLOSE.len_utf8())
}

/// Restores placeholders in streamed text whose chunks may split a placeholder: a
/// trailing unfinished `⟦…` is held back until the next chunk or `finish`.
#[derive(Debug, Default)]
pub struct StreamRehydrator {
    carry: String,
}

/// A placeholder is never longer than this (`⟦SECRET_123456789⟧`).
const MAX_PLACEHOLDER_BYTES: usize = 32;

impl StreamRehydrator {
    pub fn push(&mut self, vault: &SecretVault, chunk: &str) -> String {
        self.carry.push_str(chunk);
        // Hold back a trailing `⟦…` only while it can still become a placeholder.
        let hold = match self.carry.rfind(OPEN) {
            Some(i)
                if self.carry.len() - i < MAX_PLACEHOLDER_BYTES
                    && self.carry[i + OPEN.len_utf8()..].bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') =>
            {
                i
            }
            _ => self.carry.len(),
        };
        let ready: String = self.carry.drain(..hold).collect();
        vault.rehydrate(&ready).into_owned()
    }

    /// Whatever is still held back.
    pub fn finish(&mut self, vault: &SecretVault) -> String {
        let rest = std::mem::take(&mut self.carry);
        vault.rehydrate(&rest).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_value_same_placeholder() {
        let v = SecretVault::default();
        let a = v.placeholder("sk-123", RedactionKind::Secret);
        let b = v.placeholder("sk-123", RedactionKind::Secret);
        let c = v.placeholder("x@y.z", RedactionKind::Email);
        assert_eq!(a, b);
        assert_eq!(a, "⟦SECRET_1⟧");
        assert_eq!(c, "⟦EMAIL_1⟧");
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn rehydrates_known_placeholders_only() {
        let v = SecretVault::default();
        let p = v.placeholder("hunter2", RedactionKind::Secret);
        let text = format!("key={p} other=⟦SECRET_9⟧ broken=⟦SECRET_ ⟦");
        assert_eq!(v.rehydrate(&text), "key=hunter2 other=⟦SECRET_9⟧ broken=⟦SECRET_ ⟦");
        assert!(matches!(v.rehydrate("plain"), Cow::Borrowed(_)));
    }

    #[test]
    fn rehydrates_json_strings_and_keys() {
        let v = SecretVault::default();
        let p = v.placeholder("tok\"en", RedactionKind::Secret);
        let mut j = serde_json::json!({"content": format!("A={p}"), "list": [p.clone(), 1], p.clone(): true});
        assert_eq!(v.rehydrate_json(&mut j), 3);
        assert_eq!(j["content"], "A=tok\"en");
        assert_eq!(j["list"][0], "tok\"en");
        assert_eq!(j["tok\"en"], true);
    }

    #[test]
    fn stream_rehydration_handles_split_placeholders() {
        let v = SecretVault::default();
        let p = v.placeholder("abc", RedactionKind::Secret);
        let (left, right) = p.split_at(p.find('R').unwrap());
        let mut s = StreamRehydrator::default();
        let mut out = s.push(&v, &format!("x {left}"));
        assert_eq!(out, "x ");
        out.push_str(&s.push(&v, &format!("{right} y")));
        out.push_str(&s.finish(&v));
        assert_eq!(out, "x abc y");
    }

    #[test]
    fn stream_rehydration_releases_a_lone_open_mark() {
        let v = SecretVault::default();
        let mut s = StreamRehydrator::default();
        assert_eq!(s.push(&v, "a ⟦ not a placeholder"), "a ⟦ not a placeholder");
        assert_eq!(s.push(&v, "b ⟦SEC"), "b ");
        let mut out = s.push(&v, &"Z".repeat(40));
        out.push_str(&s.finish(&v));
        assert_eq!(out, format!("⟦SEC{}", "Z".repeat(40)));
    }
}
