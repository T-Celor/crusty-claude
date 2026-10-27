//! Language detection by file extension. Files with unknown extensions are not indexed.

/// Language name of a repository-relative path, or `None` when the file is not indexed.
pub fn language_of(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next()?;
    let (_, ext) = name.rsplit_once('.')?;
    Some(match ext.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "ts" | "tsx" => "typescript",
        "go" => "go",
        "java" => "java",
        "kt" => "kotlin",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "scala" => "scala",
        "lua" => "lua",
        "sh" | "bash" => "shell",
        "md" => "markdown",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::language_of;

    #[test]
    fn detects_languages_by_extension() {
        assert_eq!(language_of("src/main.rs"), Some("rust"));
        assert_eq!(language_of("web/App.TSX"), Some("typescript"));
        assert_eq!(language_of("docs/readme.md"), Some("markdown"));
        assert_eq!(language_of("image.png"), None);
        assert_eq!(language_of(".gitignore"), None);
        assert_eq!(language_of("dir.v2/Makefile"), None);
    }
}
