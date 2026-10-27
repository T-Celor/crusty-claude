//! Transcripts, memory files, custom commands and agents, shell hooks.
//!
//! Everything here reads or writes plain files in Claude-Code-compatible locations. No
//! function reads the home directory on its own: callers pass it in, which keeps tests
//! hermetic.

pub mod agents;
pub mod commands;
pub mod frontmatter;
pub mod hooks;
pub mod memory;
pub mod transcript;

use std::fs;
use std::path::{Path, PathBuf};

/// Shortens `text` to at most `max` characters, ending with an ellipsis when it is cut.
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Markdown files directly in `dir`, or anywhere below it when `recursive`, sorted by path.
///
/// Symlinked directories are not followed, so a symlink cycle cannot loop forever.
pub(crate) fn markdown_files(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_markdown(dir, recursive, &mut out);
    out
}

fn collect_markdown(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|entry| entry.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        if recursive && path.is_dir() && !path.is_symlink() {
            collect_markdown(&path, true, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") && path.is_file() {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_text_and_marks_cut_text() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
    }
}
