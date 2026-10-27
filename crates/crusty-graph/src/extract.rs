//! Regex extractors: definitions and imports per language. Heuristic by design (no parser),
//! so unusual formatting can be missed or misread.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

/// A named definition found in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub name: String,
    /// Flavour of the definition, such as `fn`, `struct`, `class` or `heading`.
    pub kind: &'static str,
    /// 1-based line number.
    pub line: u32,
}

/// How the resolver interprets an import string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    /// A module or package path such as `a::b`, `a.b` or `github.com/x/y`.
    Path,
    /// A path relative to the importing file, such as `./x`, `"x.h"` or `require_relative`.
    Relative,
    /// Rust `mod x;`, which names a child module file.
    ModDecl,
}

/// An import statement. `module` is the raw path as written, without trailing punctuation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub module: String,
    pub kind: ImportKind,
    pub line: u32,
}

/// Everything extracted from one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extracted {
    pub definitions: Vec<Definition>,
    pub imports: Vec<Import>,
}

#[derive(Clone)]
struct DefRule {
    re: Regex,
    kind: &'static str,
}

#[derive(Clone)]
struct ImportRule {
    re: Regex,
    kind: ImportKind,
    /// The capture is a comma-separated list (`import a, b`).
    multi: bool,
}

#[derive(Clone)]
struct Spec {
    comments: &'static [&'static str],
    defs: Vec<DefRule>,
    imports: Vec<ImportRule>,
    /// Go groups imports in `import ( ... )` blocks.
    go_block: bool,
}

/// Words that look like `name(` to the method heuristic but are control flow or expressions.
const KEYWORDS: &[&str] = &[
    "if",
    "for",
    "while",
    "switch",
    "return",
    "new",
    "else",
    "catch",
    "synchronized",
    "throw",
    "await",
    "yield",
    "case",
    "do",
    "try",
    "using",
    "lock",
    "when",
    "elif",
    "until",
    "foreach",
    "sizeof",
    "typeof",
    "and",
    "or",
    "not",
    "import",
    "package",
];

const C_COMMENTS: &[&str] = &["//", "/*", "*"];
const HASH_COMMENTS: &[&str] = &["#"];
const DASH_COMMENTS: &[&str] = &["--"];
const NO_COMMENTS: &[&str] = &[];
const VIS_RUST: &str = r"(?:pub(?:\([^)]*\))?\s+)?";
const MODS_JVM: &str = r"(?:(?:@\w+(?:\([^)]*\))?|public|private|protected|internal|static|final|abstract|sealed|partial|data|open|override|inline|suspend|async|export|virtual|readonly|const|new)\s+)*";
const GO_BLOCK_IMPORT: &str = r##"^\s*(?:[\w.]+\s+)?"([^"]+)""##;

static SPECS: LazyLock<BTreeMap<&'static str, Spec>> = LazyLock::new(build_specs);
static GO_BLOCK: LazyLock<Regex> = LazyLock::new(|| rx(GO_BLOCK_IMPORT));

/// Extracts definitions (only when `want_defs`) and imports of `source`, written in `lang`.
/// Unknown languages yield nothing.
pub fn extract(lang: &str, source: &str, want_defs: bool) -> Extracted {
    let Some(spec) = SPECS.get(lang) else {
        return Extracted::default();
    };
    let mut out = Extracted::default();
    let mut in_go_block = false;
    for (index, line) in source.lines().enumerate() {
        let line_no = u32::try_from(index + 1).unwrap_or(u32::MAX);
        let trimmed = line.trim_start();
        if spec.comments.iter().any(|prefix| trimmed.starts_with(prefix)) {
            continue;
        }
        if spec.go_block {
            if trimmed.starts_with("import (") {
                in_go_block = true;
                continue;
            }
            if in_go_block {
                if trimmed.starts_with(')') {
                    in_go_block = false;
                } else if let Some(caps) = GO_BLOCK.captures(trimmed) {
                    push_import(&mut out, &caps[1], ImportKind::Path, line_no);
                }
                continue;
            }
        }
        if want_defs {
            for rule in &spec.defs {
                let Some(caps) = rule.re.captures(line) else {
                    continue;
                };
                let Some(name) = caps.get(caps.len() - 1) else {
                    continue;
                };
                if caps.iter().skip(1).flatten().any(|m| KEYWORDS.contains(&m.as_str())) {
                    continue;
                }
                out.definitions.push(Definition { name: name.as_str().to_owned(), kind: rule.kind, line: line_no });
                break;
            }
        }
        for rule in &spec.imports {
            for caps in rule.re.captures_iter(line) {
                let Some(m) = caps.get(1) else {
                    continue;
                };
                if rule.multi {
                    for part in m.as_str().split(',') {
                        if let Some(first) = part.split_whitespace().next() {
                            push_import(&mut out, first, rule.kind, line_no);
                        }
                    }
                } else {
                    push_import(&mut out, m.as_str(), rule.kind, line_no);
                }
            }
        }
    }
    out
}

fn push_import(out: &mut Extracted, raw: &str, kind: ImportKind, line: u32) {
    let module = raw.trim().trim_end_matches(&['.', ':'][..]);
    if !module.is_empty() {
        out.imports.push(Import { module: module.to_owned(), kind, line });
    }
}

fn build_specs() -> BTreeMap<&'static str, Spec> {
    let mut m = BTreeMap::new();
    m.insert("rust", rust_spec());
    let js = js_spec();
    m.insert("javascript", js.clone());
    m.insert("typescript", js);
    m.insert("python", python_spec());
    m.insert("go", go_spec());
    let jvm = jvm_spec();
    for lang in ["java", "kotlin", "csharp"] {
        m.insert(lang, jvm.clone());
    }
    m.insert("swift", swift_spec());
    m.insert("scala", scala_spec());
    let c = c_spec();
    m.insert("c", c.clone());
    m.insert("cpp", c);
    m.insert("ruby", ruby_spec());
    m.insert("php", php_spec());
    m.insert("lua", lua_spec());
    m.insert("shell", shell_spec());
    m.insert("markdown", markdown_spec());
    m
}

/// Compiles a literal pattern. The patterns are compile-time constants, so a failure is a bug
/// in this file that the unit tests catch.
fn rx(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid literal regex")
}

fn def(pattern: &str, kind: &'static str) -> DefRule {
    DefRule { re: rx(pattern), kind }
}

fn imp(pattern: &str, kind: ImportKind) -> ImportRule {
    ImportRule { re: rx(pattern), kind, multi: false }
}

fn imp_multi(pattern: &str, kind: ImportKind) -> ImportRule {
    ImportRule { re: rx(pattern), kind, multi: true }
}

fn rust_spec() -> Spec {
    let v = VIS_RUST;
    Spec {
        comments: C_COMMENTS,
        go_block: false,
        defs: vec![
            def(
                &format!(r"^\s*{v}(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+(?:\S+\s+)?)?fn\s+([A-Za-z_]\w*)"),
                "fn",
            ),
            def(&format!(r"^\s*{v}struct\s+([A-Za-z_]\w*)"), "struct"),
            def(&format!(r"^\s*{v}enum\s+([A-Za-z_]\w*)"), "enum"),
            def(&format!(r"^\s*{v}(?:unsafe\s+)?trait\s+([A-Za-z_]\w*)"), "trait"),
            def(&format!(r"^\s*{v}type\s+([A-Za-z_]\w*)"), "type"),
            def(&format!(r"^\s*{v}const\s+([A-Za-z_]\w*)\s*:"), "const"),
            def(&format!(r"^\s*{v}static\s+(?:mut\s+)?([A-Za-z_]\w*)\s*:"), "static"),
            def(&format!(r"^\s*{v}mod\s+([A-Za-z_]\w*)"), "mod"),
            def(r"^\s*macro_rules!\s*([A-Za-z_]\w*)", "macro"),
            def(r"^\s*(?:unsafe\s+)?impl\b\s*(?:<[^>]*>\s*)?([^{]+?)\s*(?:\{|where\b|$)", "impl"),
        ],
        imports: vec![
            imp(&format!(r"^\s*{v}use\s+([\w:]+)"), ImportKind::Path),
            imp(&format!(r"^\s*{v}mod\s+([A-Za-z_]\w*)\s*;"), ImportKind::ModDecl),
        ],
    }
}

fn python_spec() -> Spec {
    Spec {
        comments: HASH_COMMENTS,
        go_block: false,
        defs: vec![def(r"^\s*(?:async\s+)?def\s+([A-Za-z_]\w*)", "fn"), def(r"^\s*class\s+([A-Za-z_]\w*)", "class")],
        imports: vec![imp_multi(r"^\s*import\s+(.+)$", ImportKind::Path), imp(r"^\s*from\s+(\.*[\w.]*)\s+import\b", ImportKind::Path)],
    }
}

fn js_spec() -> Spec {
    Spec {
        comments: C_COMMENTS,
        go_block: false,
        defs: vec![
            def(r"^\s*(?:export\s+)?(?:default\s+)?(?:declare\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)", "function"),
            def(r"^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)", "class"),
            def(r"^\s*(?:export\s+)?(?:declare\s+)?interface\s+([A-Za-z_$][\w$]*)", "interface"),
            def(r"^\s*(?:export\s+)?(?:declare\s+)?type\s+([A-Za-z_$][\w$]*)\s*(?:<[^=]*>)?\s*=", "type"),
            def(r"^\s*(?:export\s+)?(?:const\s+)?enum\s+([A-Za-z_$][\w$]*)", "enum"),
            def(
                r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s+)?(?:function\b|\([^)]*\)\s*(?::[^=]+)?=>|[A-Za-z_$][\w$]*\s*=>)",
                "function",
            ),
        ],
        imports: vec![
            imp(r#"\bfrom\s*['"]([^'"]+)['"]"#, ImportKind::Path),
            imp(r#"\bimport\s*\(?\s*['"]([^'"]+)['"]"#, ImportKind::Path),
            imp(r#"\brequire\(\s*['"]([^'"]+)['"]\s*\)"#, ImportKind::Path),
        ],
    }
}

fn go_spec() -> Spec {
    Spec {
        comments: &["//"],
        go_block: true,
        defs: vec![def(r"^func\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)", "func"), def(r"^type\s+([A-Za-z_]\w*)", "type")],
        imports: vec![imp(r##"^\s*import\s+(?:[\w.]+\s+)?"([^"]+)""##, ImportKind::Path)],
    }
}

/// Java, Kotlin and C# share one heuristic: declarations behind optional modifiers.
fn jvm_spec() -> Spec {
    let m = MODS_JVM;
    Spec {
        comments: C_COMMENTS,
        go_block: false,
        defs: vec![
            def(&format!(r"^\s*{m}class\s+([A-Za-z_]\w*)"), "class"),
            def(&format!(r"^\s*{m}interface\s+([A-Za-z_]\w*)"), "interface"),
            def(&format!(r"^\s*{m}enum\s+([A-Za-z_]\w*)"), "enum"),
            def(&format!(r"^\s*{m}record\s+([A-Za-z_]\w*)\s*[(<]"), "record"),
            def(&format!(r"^\s*{m}object\s+([A-Za-z_]\w*)"), "object"),
            def(&format!(r"^\s*{m}struct\s+([A-Za-z_]\w*)"), "struct"),
            def(&format!(r"^\s*{m}fun\s+(?:<[^>]*>\s*)?(?:[\w.]+\.)?([A-Za-z_]\w*)\s*\("), "fn"),
            def(&format!(r"^\s*{m}([A-Za-z_][\w<>\[\],.?]*)\s+([A-Za-z_]\w*)\s*\("), "method"),
        ],
        imports: vec![
            imp(r"^\s*import\s+(?:static\s+)?([\w.]+)", ImportKind::Path),
            imp(r"^\s*using\s+(?:static\s+)?([\w.]+)\s*;", ImportKind::Path),
        ],
    }
}

fn swift_spec() -> Spec {
    let mut defs =
        vec![def(r"^\s*(?:(?:public|private|internal|fileprivate|open|static|final|override|mutating)\s+)*func\s+([A-Za-z_]\w*)", "fn")];
    for kind in ["class", "struct", "enum", "protocol"] {
        defs.push(def(&format!(r"^\s*(?:(?:public|private|internal|final|open)\s+)*{kind}\s+([A-Za-z_]\w*)"), kind));
    }
    Spec { comments: C_COMMENTS, go_block: false, defs, imports: vec![imp(r"^\s*import\s+([A-Za-z_]\w*)", ImportKind::Path)] }
}

fn scala_spec() -> Spec {
    let mut defs =
        vec![def(r"^\s*(?:(?:private|protected|override|final|sealed|implicit|case|abstract|lazy)\s+)*def\s+([A-Za-z_]\w*)", "fn")];
    for kind in ["class", "object", "trait"] {
        defs.push(def(&format!(r"^\s*(?:(?:private|protected|final|sealed|abstract|case|implicit)\s+)*{kind}\s+([A-Za-z_]\w*)"), kind));
    }
    Spec { comments: C_COMMENTS, go_block: false, defs, imports: vec![imp(r"^\s*import\s+([\w.]+)", ImportKind::Path)] }
}

fn c_spec() -> Spec {
    Spec {
        comments: C_COMMENTS,
        go_block: false,
        defs: vec![
            def(r"^([A-Za-z_][\w\s\*&:<>,]*?[\s\*&]+)([A-Za-z_]\w*)\s*\([^;]*$", "fn"),
            def(r"^\s*(?:typedef\s+)?struct\s+([A-Za-z_]\w*)\s*(?:\{|:|$)", "struct"),
            def(r"^\s*(?:typedef\s+)?union\s+([A-Za-z_]\w*)\s*(?:\{|:|$)", "union"),
            def(r"^\s*(?:typedef\s+)?enum\s+([A-Za-z_]\w*)\s*(?:\{|:|$)", "enum"),
            def(r"^\s*class\s+([A-Za-z_]\w*)\s*(?:\{|:|$)", "class"),
        ],
        imports: vec![imp(r#"^\s*#\s*include\s*"([^"]+)""#, ImportKind::Relative), imp(r"^\s*#\s*include\s*<([^>]+)>", ImportKind::Path)],
    }
}

fn ruby_spec() -> Spec {
    Spec {
        comments: HASH_COMMENTS,
        go_block: false,
        defs: vec![
            def(r"^\s*def\s+(?:self\.)?([A-Za-z_]\w*[?!=]?)", "fn"),
            def(r"^\s*class\s+([A-Za-z_][\w:]*)", "class"),
            def(r"^\s*module\s+([A-Za-z_][\w:]*)", "module"),
        ],
        imports: vec![
            imp(r#"^\s*require_relative\s*\(?\s*['"]([^'"]+)['"]"#, ImportKind::Relative),
            imp(r#"^\s*require\s*\(?\s*['"]([^'"]+)['"]"#, ImportKind::Path),
        ],
    }
}

fn php_spec() -> Spec {
    Spec {
        comments: &["//", "#", "/*", "*"],
        go_block: false,
        defs: vec![
            def(r"^\s*(?:(?:abstract|final)\s+)?class\s+([A-Za-z_]\w*)", "class"),
            def(r"^\s*interface\s+([A-Za-z_]\w*)", "interface"),
            def(r"^\s*trait\s+([A-Za-z_]\w*)", "trait"),
            def(r"^\s*(?:(?:public|private|protected|static|final|abstract)\s+)*function\s+&?([A-Za-z_]\w*)", "fn"),
        ],
        imports: vec![
            imp(r"^\s*use\s+([\w\\]+)", ImportKind::Path),
            imp(r#"^\s*(?:require|include)(?:_once)?\s*\(?\s*['"]([^'"]+)['"]"#, ImportKind::Relative),
        ],
    }
}

fn lua_spec() -> Spec {
    Spec {
        comments: DASH_COMMENTS,
        go_block: false,
        defs: vec![def(r"^\s*(?:local\s+)?function\s+([A-Za-z_][\w.:]*)", "fn")],
        imports: vec![imp(r#"\brequire\s*\(?\s*['"]([^'"]+)['"]"#, ImportKind::Path)],
    }
}

fn shell_spec() -> Spec {
    Spec {
        comments: HASH_COMMENTS,
        go_block: false,
        defs: vec![def(r"^\s*function\s+([A-Za-z_][\w-]*)", "fn"), def(r"^\s*([A-Za-z_][\w-]*)\s*\(\s*\)", "fn")],
        imports: vec![imp(r#"^\s*(?:source|\.)\s+['"]?([^'"\s;]+)"#, ImportKind::Relative)],
    }
}

fn markdown_spec() -> Spec {
    Spec {
        comments: NO_COMMENTS,
        go_block: false,
        defs: vec![def(r"^#{1,2}\s+(.+?)\s*$", "heading")],
        imports: vec![imp(r"\]\(([^()\s#:]+\.md)", ImportKind::Relative)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs(lang: &str, src: &str) -> Vec<(String, &'static str, u32)> {
        extract(lang, src, true).definitions.into_iter().map(|d| (d.name, d.kind, d.line)).collect()
    }

    #[test]
    fn rust_definitions_and_imports() {
        let src =
            "mod util;\nuse crate::util::{helper, Widget};\npub(crate) fn run() {}\npub struct Widget;\nimpl Display for Widget {\n}\n";
        let out = extract("rust", src, true);
        let names: Vec<_> = out.definitions.iter().map(|d| (d.name.as_str(), d.kind, d.line)).collect();
        assert!(names.contains(&("run", "fn", 3)));
        assert!(names.contains(&("Widget", "struct", 4)));
        assert!(names.contains(&("Display for Widget", "impl", 5)));
        assert!(out.imports.iter().any(|i| i.module == "crate::util" && i.kind == ImportKind::Path));
        assert!(out.imports.iter().any(|i| i.module == "util" && i.kind == ImportKind::ModDecl));
    }

    #[test]
    fn python_async_defs_and_import_lists() {
        let src = "import os, pkg.b as pb\nfrom pkg.a import X\n# def hidden():\nasync def run():\n    pass\n";
        assert_eq!(defs("python", src), vec![("run".to_owned(), "fn", 4)]);
        let modules: Vec<String> = extract("python", src, false).imports.into_iter().map(|i| i.module).collect();
        assert_eq!(modules, vec!["os", "pkg.b", "pkg.a"]);
    }

    #[test]
    fn go_import_block_and_functions() {
        let src = "import (\n    \"fmt\"\n    log \"github.com/x/log\"\n)\n\nfunc (s *Store) Save() {}\n";
        let out = extract("go", src, true);
        let modules: Vec<&str> = out.imports.iter().map(|i| i.module.as_str()).collect();
        assert_eq!(modules, vec!["fmt", "github.com/x/log"]);
        assert_eq!(out.definitions[0].name, "Save");
    }

    #[test]
    fn markdown_headings_and_local_links() {
        let src = "# Title\n## Setup\n### Deep\nSee [guide](docs/guide.md) and [web](https://x.io/a.md).\n";
        let out = extract("markdown", src, true);
        let heads: Vec<&str> = out.definitions.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(heads, vec!["Title", "Setup"]);
        assert_eq!(out.imports.len(), 1);
        assert_eq!(out.imports[0].module, "docs/guide.md");
    }
}
