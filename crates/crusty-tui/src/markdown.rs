//! Light markdown rendering for assistant text: headings, code fences, bullet lists and
//! inline code. Wrapping is left to the paragraph widget.

use ratatui::text::{Line, Span};

use crate::theme;

/// Renders markdown-like text into styled lines.
pub fn render(text: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_code = false;
    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            out.push(Line::styled(raw.to_string(), theme::dim()));
        } else if in_code {
            out.push(Line::styled(raw.to_string(), theme::code()));
        } else if let Some(heading) = heading(trimmed) {
            out.push(Line::from(Span::styled(heading.to_string(), theme::bold())));
        } else if let Some(item) = bullet(trimmed) {
            let indent = raw.len() - trimmed.len();
            let mut spans = vec![Span::raw(" ".repeat(indent)), Span::styled("• ", theme::code())];
            spans.extend(inline(item));
            out.push(Line::from(spans));
        } else {
            out.push(Line::from(inline(raw)));
        }
    }
    out
}

/// The text of an ATX heading (`# Title`), without the hashes.
fn heading(line: &str) -> Option<&str> {
    let text = line.trim_start_matches('#');
    (text.len() < line.len() && text.starts_with(' ')).then(|| text.trim())
}

/// The text of a bullet item (`- item` or `* item`).
fn bullet(line: &str) -> Option<&str> {
    line.strip_prefix("- ").or_else(|| line.strip_prefix("* "))
}

/// Splits on backticks: the odd-numbered parts are inline code.
fn inline(text: &str) -> Vec<Span<'static>> {
    text.split('`')
        .enumerate()
        .filter(|(_, part)| !part.is_empty())
        .map(|(i, part)| if i % 2 == 1 { Span::styled(part.to_string(), theme::code()) } else { Span::raw(part.to_string()) })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    #[test]
    fn headings_are_bold_and_fences_are_code() {
        let lines = render("# Title\n```\nlet x = 1;\n```\nafter");
        assert_eq!(lines[0].spans[0].content, "Title");
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(lines[1].style, theme::dim());
        assert_eq!(lines[2].style, theme::code());
        assert_eq!(lines[4].spans[0].content, "after");
    }

    #[test]
    fn inline_code_is_highlighted_and_bullets_get_a_dot() {
        let lines = render("- use `cargo test` now");
        let texts: Vec<&str> = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, vec!["", "• ", "use ", "cargo test", " now"]);
        assert_eq!(lines[0].spans[3].style, theme::code());
    }
}
