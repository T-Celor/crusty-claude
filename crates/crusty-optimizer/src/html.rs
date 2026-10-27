//! HTML page to Markdown for the model: metadata read from the raw HTML, page chrome
//! removed, the body converted with `html-to-markdown-rs`, then capped in length.

use std::sync::LazyLock;

use crusty_core::filter::HtmlDoc;
use html_to_markdown_rs::{ConversionOptions, convert};
use regex::Regex;

// Literal patterns: a compile failure is a bug caught by the tests.
const STRIPPED_TAGS: [&str; 7] = ["script", "style", "noscript", "svg", "nav", "footer", "iframe"];

static STRIPPED: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    STRIPPED_TAGS.iter().map(|tag| Regex::new(&format!(r"(?is)<{tag}\b[^>]*?(/>|>.*?</{tag}\s*>)")).expect("valid block pattern")).collect()
});
static TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title\s*>").expect("valid title pattern"));
static META: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<meta\b[^>]*>").expect("valid meta pattern"));
static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<link\b[^>]*>").expect("valid link pattern"));
static TIME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<time\b[^>]*>").expect("valid time pattern"));
static ATTR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)([a-zA-Z:_-]+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).expect("valid attribute pattern"));
static BLANK_LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").expect("valid blank-line pattern"));

/// Converts a page. `None` when conversion fails or the result is empty.
///
/// `max_chars` caps the Markdown; the cut is made at a line boundary and says how much was
/// dropped. The metadata is read from the original HTML, before any removal.
pub fn convert_html(html: &str, url: &str, max_chars: usize) -> Option<HtmlDoc> {
    tracing::debug!(url, "converting html page");
    let title = find_title(html);
    let canonical_url = link_canonical(html).or_else(|| meta_value(html, &["og:url"]));
    let description = meta_value(html, &["description", "og:description"]);
    let published = meta_value(html, &["article:published_time", "date", "pubdate"]).or_else(|| time_datetime(html));

    let mut cleaned = html.to_owned();
    for re in STRIPPED.iter() {
        cleaned = re.replace_all(&cleaned, "").into_owned();
    }
    let content = convert(&cleaned, ConversionOptions::default()).ok()?.content?;
    let markdown = BLANK_LINES.replace_all(content.trim(), "\n\n").into_owned();
    if markdown.is_empty() {
        return None;
    }
    Some(HtmlDoc {
        markdown: truncate_at_line(markdown, max_chars),
        title: title.or_else(|| meta_value(html, &["og:title"])),
        canonical_url,
        description,
        published,
    })
}

fn find_title(html: &str) -> Option<String> {
    let raw = TITLE.captures(html)?.get(1)?.as_str();
    non_empty(decode_entities(raw.trim()))
}

fn non_empty(s: String) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// Attributes of one tag, with names lower-cased.
fn attributes(tag: &str) -> Vec<(String, String)> {
    ATTR.captures_iter(tag)
        .filter_map(|c| {
            let value = c.get(2).or_else(|| c.get(3))?.as_str();
            Some((c[1].to_ascii_lowercase(), decode_entities(value)))
        })
        .collect()
}

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// `content` of the first `<meta>` whose `name` or `property` is one of `keys`, in key order.
fn meta_value(html: &str, keys: &[&str]) -> Option<String> {
    let metas: Vec<Vec<(String, String)>> = META.find_iter(html).map(|m| attributes(m.as_str())).collect();
    keys.iter().find_map(|key| {
        metas.iter().find_map(|attrs| {
            let named = attr(attrs, "name") == Some(*key) || attr(attrs, "property") == Some(*key);
            if named { attr(attrs, "content").map(|c| c.trim().to_owned()).and_then(non_empty) } else { None }
        })
    })
}

fn link_canonical(html: &str) -> Option<String> {
    LINK.find_iter(html).find_map(|m| {
        let attrs = attributes(m.as_str());
        if attr(&attrs, "rel")?.eq_ignore_ascii_case("canonical") { non_empty(attr(&attrs, "href")?.to_owned()) } else { None }
    })
}

fn time_datetime(html: &str) -> Option<String> {
    TIME.find_iter(html).find_map(|m| non_empty(attr(&attributes(m.as_str()), "datetime")?.to_owned()))
}

/// Decodes the entities that occur in titles and metadata.
fn decode_entities(s: &str) -> String {
    // `&amp;` goes last so `&amp;lt;` decodes to `&lt;`, not `<`.
    s.replace("&quot;", "\"").replace("&#39;", "'").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// Cuts at the last line break within `max_chars` and appends how many characters were left.
fn truncate_at_line(text: String, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text;
    }
    let limit = text.char_indices().nth(max_chars).map_or(text.len(), |(i, _)| i);
    let cut = text[..limit].rfind('\n').unwrap_or(limit);
    let kept = text[..cut].trim_end();
    let omitted = total - kept.chars().count();
    format!("{kept}\n[… truncated: {omitted} more characters]")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!doctype html>
<html><head>
<title>Rust &amp; Cargo</title>
<meta name="description" content="A guide to the toolchain">
<meta property="og:title" content="OG title">
<meta property="article:published_time" content="2025-03-04T10:00:00Z">
<link rel="canonical" href="https://example.com/guide">
<style>body { color: red; }</style>
<script>var x = "<p>nope</p>";</script>
</head>
<body>
<nav><a href="/home">Home menu</a></nav>
<article>
<h1>Guide</h1>
<p>Read the <a href="https://doc.rust-lang.org">docs</a> first.</p>
<table><tr><th>Name</th><th>Size</th></tr><tr><td>alpha</td><td>10</td></tr></table>
<pre><code class="language-rust">fn main() { println!("hi"); }</code></pre>
</article>
<footer>Copyright footer text</footer>
<iframe src="https://ads.example.com">ad frame</iframe>
</body></html>"#;

    #[test]
    fn extracts_metadata() {
        let doc = convert_html(PAGE, "https://example.com/guide", 100_000).unwrap();
        assert_eq!(doc.title.as_deref(), Some("Rust & Cargo"));
        assert_eq!(doc.canonical_url.as_deref(), Some("https://example.com/guide"));
        assert_eq!(doc.description.as_deref(), Some("A guide to the toolchain"));
        assert_eq!(doc.published.as_deref(), Some("2025-03-04T10:00:00Z"));
    }

    #[test]
    fn title_falls_back_to_og_and_published_to_time_element() {
        let html = r#"<meta property="og:title" content="Only OG"><meta property="og:url" content="https://x.io/p"><time datetime="2024-01-02">Jan</time><p>Body text</p>"#;
        let doc = convert_html(html, "https://x.io/p", 1000).unwrap();
        assert_eq!(doc.title.as_deref(), Some("Only OG"));
        assert_eq!(doc.canonical_url.as_deref(), Some("https://x.io/p"));
        assert_eq!(doc.published.as_deref(), Some("2024-01-02"));
    }

    #[test]
    fn removes_scripts_styles_and_page_chrome() {
        let doc = convert_html(PAGE, "u", 100_000).unwrap();
        assert!(doc.markdown.contains("Guide"));
        assert!(!doc.markdown.contains("color: red"));
        assert!(!doc.markdown.contains("nope"));
        assert!(!doc.markdown.contains("Home menu"));
        assert!(!doc.markdown.contains("Copyright footer"));
        assert!(!doc.markdown.contains("ad frame"));
    }

    #[test]
    fn keeps_links_tables_and_code_blocks() {
        let doc = convert_html(PAGE, "u", 100_000).unwrap();
        assert!(doc.markdown.contains("[docs](https://doc.rust-lang.org)"), "{}", doc.markdown);
        assert!(doc.markdown.contains("| alpha"), "{}", doc.markdown);
        assert!(doc.markdown.contains("println!"), "{}", doc.markdown);
        assert!(doc.markdown.contains("```"), "{}", doc.markdown);
    }

    #[test]
    fn truncates_at_a_line_boundary_with_note() {
        let body: String = (0..200).map(|i| format!("<p>paragraph {i} with some filler words</p>")).collect();
        let doc = convert_html(&body, "u", 300).unwrap();
        assert!(doc.markdown.contains("[… truncated:"));
        let kept = doc.markdown.split("\n[… truncated").next().unwrap();
        assert!(kept.chars().count() <= 300);
        assert!(kept.lines().filter(|l| !l.is_empty()).all(|l| l.starts_with("paragraph")));
    }

    #[test]
    fn entities_decode_in_one_pass_with_amp_last() {
        assert_eq!(decode_entities("&amp;lt;b&amp;gt;"), "&lt;b&gt;");
        assert_eq!(decode_entities("Tom &amp; Jerry &lt;3"), "Tom & Jerry <3");
    }

    #[test]
    fn empty_page_is_none() {
        assert!(convert_html("<script>only()</script>", "u", 1000).is_none());
    }
}
