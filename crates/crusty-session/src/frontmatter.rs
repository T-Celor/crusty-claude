//! Minimal `---` frontmatter parser for memory, command and agent files. There is no YAML:
//! only flat `key: value` lines are understood.

use std::collections::BTreeMap;

/// Splits an optional leading `---` block of `key: value` lines from `text`.
///
/// Returns the fields and the body after the closing `---`. Text without a closed block is
/// returned whole, with no fields. Values are unquoted; list syntax such as `[a, b]` is kept
/// raw for [`list`] to split.
pub fn parse(text: &str) -> (BTreeMap<String, String>, &str) {
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        return (BTreeMap::new(), text);
    }
    let mut fields = BTreeMap::new();
    let mut pos = text.find('\n').map_or(text.len(), |i| i + 1);
    for line in lines {
        pos += line.len();
        let trimmed = line.trim();
        if trimmed == "---" {
            return (fields, &text[pos..]);
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            fields.insert(key.trim().to_owned(), unquote(value.trim()).to_owned());
        }
    }
    (BTreeMap::new(), text)
}

/// Splits a list value (`[a, b]`, `a, b` or `a b`) into trimmed, unquoted items.
///
/// Commas win over whitespace, so `Bash(git add:*), Read` keeps its inner spaces.
pub fn list(value: &str) -> Vec<String> {
    let value = value.trim();
    let inner = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')).unwrap_or(value);
    let items: Vec<&str> = if inner.contains(',') { inner.split(',').collect() } else { inner.split_whitespace().collect() };
    items.into_iter().map(|item| unquote(item.trim()).to_owned()).filter(|item| !item.is_empty()).collect()
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value.strip_prefix(quote).and_then(|v| v.strip_suffix(quote)) {
            return inner;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_fields_and_body() {
        let text = "---\nname: \"reviewer\"\ntools: [Read, Grep]\n# comment\n---\nBody line\n";
        let (fields, body) = parse(text);
        assert_eq!(fields.get("name").map(String::as_str), Some("reviewer"));
        assert_eq!(fields.get("tools").map(String::as_str), Some("[Read, Grep]"));
        assert_eq!(fields.len(), 2);
        assert_eq!(body, "Body line\n");
    }

    #[test]
    fn text_without_frontmatter_is_all_body() {
        let (fields, body) = parse("# Title\n");
        assert!(fields.is_empty());
        assert_eq!(body, "# Title\n");
    }

    #[test]
    fn unclosed_block_is_plain_text() {
        let text = "---\nname: x\nno closing line\n";
        let (fields, body) = parse(text);
        assert!(fields.is_empty());
        assert_eq!(body, text);
    }

    #[test]
    fn list_accepts_brackets_commas_and_spaces() {
        assert_eq!(list("[a, b]"), ["a", "b"]);
        assert_eq!(list("a, b"), ["a", "b"]);
        assert_eq!(list("read write"), ["read", "write"]);
        assert_eq!(list("Bash(git add:*), Read"), ["Bash(git add:*)", "Read"]);
        assert!(list("[]").is_empty());
    }
}
