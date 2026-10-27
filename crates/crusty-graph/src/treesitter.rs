//! Tree-sitter extractor: the same [`Extracted`] model as the regex extractor in
//! [`crate::extract`], read from a parse tree instead of single lines.
//!
//! One query string per grammar (compiled once, see [`GRAMMARS`]) collects definitions and
//! imports. Identifier leaves of the tree feed the reference edges. Covered languages: Rust,
//! Python, JavaScript, TypeScript (`.ts`, and `.tsx` through the TSX grammar), Go, Java, C and
//! C++. Other languages keep the regex extractor. Parse errors are tolerated: the partial tree
//! yields what it can, and nothing panics.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use tree_sitter::{Language, Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

use crate::extract::{Definition, Extracted, ImportKind, push_import};

/// Definition flavours a query may capture. A capture named otherwise is not a definition.
const KINDS: &[&str] = &[
    "fn",
    "func",
    "struct",
    "enum",
    "union",
    "trait",
    "type",
    "const",
    "static",
    "mod",
    "macro",
    "impl",
    "class",
    "interface",
    "record",
    "method",
    "function",
    "namespace",
];

const RUST_DEFS: &str = r#"
(function_item name: (identifier) @name) @fn
(struct_item name: (type_identifier) @name) @struct
(enum_item name: (type_identifier) @name) @enum
(trait_item name: (type_identifier) @name) @trait
(type_item name: (type_identifier) @name) @type
(const_item name: (identifier) @name) @const
(static_item name: (identifier) @name) @static
(mod_item name: (identifier) @name) @mod
(macro_definition name: (identifier) @name) @macro
(impl_item) @impl
"#;

const RUST_IMPORTS: &str = r#"
(use_declaration argument: (_) @import.path)
(mod_item name: (identifier) @import.mod !body)
"#;

const PYTHON_DEFS: &str = r#"
(function_definition name: (identifier) @name) @fn
(class_definition name: (identifier) @name) @class
"#;

const PYTHON_IMPORTS: &str = r#"
(import_statement name: (dotted_name) @import.path)
(import_statement name: (aliased_import name: (dotted_name) @import.path))
(import_from_statement module_name: (dotted_name) @import.path)
(import_from_statement module_name: (relative_import) @import.path)
"#;

const JS_DEFS: &str = r#"
(function_declaration name: (identifier) @name) @function
(generator_function_declaration name: (identifier) @name) @function
(class_declaration name: (identifier) @name) @class
(method_definition name: (property_identifier) @name) @function
(variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @function
"#;

const TS_DEFS: &str = r#"
(function_declaration name: (identifier) @name) @function
(generator_function_declaration name: (identifier) @name) @function
(class_declaration name: (type_identifier) @name) @class
(abstract_class_declaration name: (type_identifier) @name) @class
(interface_declaration name: (type_identifier) @name) @interface
(type_alias_declaration name: (type_identifier) @name) @type
(enum_declaration name: (identifier) @name) @enum
(method_definition name: (property_identifier) @name) @function
(variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @function
"#;

/// Shared by JavaScript and TypeScript: `import`, `export ... from`, `require()` and `import()`.
const JS_IMPORTS: &str = r#"
(import_statement source: (string (string_fragment) @import.path))
(export_statement source: (string (string_fragment) @import.path))
(call_expression function: (identifier) @callee arguments: (arguments . (string (string_fragment) @import.path)) (#eq? @callee "require"))
(call_expression function: (import) arguments: (arguments . (string (string_fragment) @import.path)))
"#;

const GO_DEFS: &str = r#"
(function_declaration name: (identifier) @name) @func
(method_declaration name: (field_identifier) @name) @func
(type_spec name: (type_identifier) @name) @type
"#;

const GO_IMPORTS: &str = r#"
(import_spec path: (interpreted_string_literal) @import.path)
"#;

const JAVA_DEFS: &str = r#"
(class_declaration name: (identifier) @name) @class
(interface_declaration name: (identifier) @name) @interface
(enum_declaration name: (identifier) @name) @enum
(record_declaration name: (identifier) @name) @record
(method_declaration name: (identifier) @name) @method
"#;

const JAVA_IMPORTS: &str = r#"
(import_declaration [(scoped_identifier) (identifier)] @import.path)
"#;

const C_DEFS: &str = r#"
(function_definition declarator: (function_declarator declarator: (identifier) @name)) @fn
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))) @fn
(struct_specifier name: (type_identifier) @name body: (field_declaration_list)) @struct
(union_specifier name: (type_identifier) @name body: (field_declaration_list)) @union
(enum_specifier name: (type_identifier) @name body: (enumerator_list)) @enum
"#;

/// C++ adds methods, qualified out-of-line definitions, classes and namespaces. Templates need
/// no pattern of their own: the templated declaration is matched where it sits.
const CPP_DEFS: &str = r#"
(function_definition declarator: (function_declarator declarator: (field_identifier) @name)) @fn
(function_definition declarator: (function_declarator declarator: (qualified_identifier name: (identifier) @name))) @fn
(class_specifier name: (type_identifier) @name body: (field_declaration_list)) @class
(namespace_definition name: (namespace_identifier) @name) @namespace
"#;

const C_IMPORTS: &str = r#"
(preproc_include path: (string_literal) @import.relative)
(preproc_include path: (system_lib_string) @import.path)
"#;

/// One grammar with its compiled queries.
struct Grammar {
    language: Language,
    defs: Query,
    imports: Query,
    /// Rust `use` arguments are trees (`a::{b, c}`); the module is the path before the group.
    rust_uses: bool,
}

/// Grammars by lookup key (`typescript`, plus `tsx` for `.tsx` files). A grammar whose queries
/// fail to compile is left out, so its language falls back to the regex extractor.
static GRAMMARS: LazyLock<BTreeMap<&'static str, Grammar>> = LazyLock::new(build_grammars);

/// Extracts `source` with the tree-sitter grammar of `lang`. `tsx` selects the TSX grammar for
/// TypeScript. Returns `None` when no grammar covers `lang`, so the caller uses the regex
/// extractor instead. Definitions are collected only when `want_defs` is set.
pub fn extract(lang: &str, tsx: bool, source: &str, want_defs: bool) -> Option<Extracted> {
    let key = if tsx && lang == "typescript" { "tsx" } else { lang };
    let grammar = GRAMMARS.get(key)?;
    let mut parser = Parser::new();
    parser.set_language(&grammar.language).ok()?;
    let tree = parser.parse(source, None)?;
    let bytes = source.as_bytes();
    let mut out = Extracted::default();
    if want_defs {
        collect_definitions(grammar, &tree, bytes, &mut out);
    }
    collect_imports(grammar, &tree, bytes, &mut out);
    collect_identifiers(tree.root_node(), bytes, &mut out);
    Some(out)
}

fn build_grammars() -> BTreeMap<&'static str, Grammar> {
    let cpp_defs = format!("{C_DEFS}\n{CPP_DEFS}");
    let entries = [
        ("rust", grammar(tree_sitter_rust::LANGUAGE.into(), RUST_DEFS, RUST_IMPORTS, true)),
        ("python", grammar(tree_sitter_python::LANGUAGE.into(), PYTHON_DEFS, PYTHON_IMPORTS, false)),
        ("javascript", grammar(tree_sitter_javascript::LANGUAGE.into(), JS_DEFS, JS_IMPORTS, false)),
        ("typescript", grammar(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), TS_DEFS, JS_IMPORTS, false)),
        ("tsx", grammar(tree_sitter_typescript::LANGUAGE_TSX.into(), TS_DEFS, JS_IMPORTS, false)),
        ("go", grammar(tree_sitter_go::LANGUAGE.into(), GO_DEFS, GO_IMPORTS, false)),
        ("java", grammar(tree_sitter_java::LANGUAGE.into(), JAVA_DEFS, JAVA_IMPORTS, false)),
        ("c", grammar(tree_sitter_c::LANGUAGE.into(), C_DEFS, C_IMPORTS, false)),
        ("cpp", grammar(tree_sitter_cpp::LANGUAGE.into(), &cpp_defs, C_IMPORTS, false)),
    ];
    entries.into_iter().filter_map(|(name, built)| built.map(|g| (name, g))).collect()
}

fn grammar(language: Language, defs: &str, imports: &str, rust_uses: bool) -> Option<Grammar> {
    let defs = Query::new(&language, defs).ok()?;
    let imports = Query::new(&language, imports).ok()?;
    Some(Grammar { language, defs, imports, rust_uses })
}

fn collect_definitions(grammar: &Grammar, tree: &Tree, source: &[u8], out: &mut Extracted) {
    let names = grammar.defs.capture_names();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&grammar.defs, tree.root_node(), source);
    while let Some(found) = matches.next() {
        let mut flavour = None;
        let mut name_node = None;
        for capture in found.captures {
            let Some(&cname) = names.get(capture.index as usize) else {
                continue;
            };
            if cname == "name" {
                name_node = Some(capture.node);
            } else if let Some(kind) = KINDS.iter().copied().find(|k| *k == cname) {
                flavour = Some((kind, capture.node));
            }
        }
        let Some((kind, node)) = flavour else {
            continue;
        };
        let (name, row) = if kind == "impl" {
            (impl_name(node, source), node.start_position().row)
        } else {
            let Some(name) = name_node else {
                continue;
            };
            (name.utf8_text(source).ok().map(str::to_owned), name.start_position().row)
        };
        let Some(name) = name.filter(|n| !n.is_empty()) else {
            continue;
        };
        out.definitions.push(Definition { name, kind, line: line_number(row) });
    }
}

/// Name of a Rust impl block as the regex extractor spells it: `Trait for Type` or `Type`.
fn impl_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let text = |n: Node<'_>| n.utf8_text(source).ok().map(str::to_owned);
    let ty = text(node.child_by_field_name("type")?)?;
    match node.child_by_field_name("trait") {
        Some(trait_node) => Some(format!("{} for {ty}", text(trait_node)?)),
        None => Some(ty),
    }
}

fn collect_imports(grammar: &Grammar, tree: &Tree, source: &[u8], out: &mut Extracted) {
    let names = grammar.imports.capture_names();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&grammar.imports, tree.root_node(), source);
    while let Some(found) = matches.next() {
        for capture in found.captures {
            let Some(&cname) = names.get(capture.index as usize) else {
                continue;
            };
            let kind = match cname {
                "import.path" => ImportKind::Path,
                "import.relative" => ImportKind::Relative,
                "import.mod" => ImportKind::ModDecl,
                _ => continue,
            };
            let node = if grammar.rust_uses { rust_use_module(capture.node) } else { Some(capture.node) };
            let Some(node) = node else {
                continue;
            };
            let Ok(text) = node.utf8_text(source) else {
                continue;
            };
            let line = line_number(capture.node.start_position().row);
            push_import(out, strip_delimiters(text), kind, line);
        }
    }
}

/// The module a Rust `use` argument names: `a::{b, c}` names `a`, `a::b as c` names `a::b`,
/// and `a::*` names `a`. A bare brace list `{a, b}` names nothing.
fn rust_use_module(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "use_list" => None,
        "scoped_use_list" | "use_as_clause" => node.child_by_field_name("path"),
        "use_wildcard" => node.named_child(0),
        _ => Some(node),
    }
}

/// Removes the quotes or angle brackets around an include or import string.
fn strip_delimiters(text: &str) -> &str {
    text.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '<' | '>'))
}

/// Records every leaf whose kind is an identifier, as the regex extractor's reference scan does.
fn collect_identifiers(root: Node<'_>, source: &[u8], out: &mut Extracted) {
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.child_count() == 0
            && node.kind().ends_with("identifier")
            && let Ok(text) = node.utf8_text(source)
            && is_reference_name(text)
        {
            out.identifiers.insert(text.to_owned());
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

/// Same shape as the regex extractor's identifier pattern: ASCII, not starting with a digit,
/// at least four characters.
fn is_reference_name(name: &str) -> bool {
    name.len() >= 4
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && name.bytes().next().is_some_and(|b| !b.is_ascii_digit())
}

fn line_number(row: usize) -> u32 {
    u32::try_from(row + 1).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{ImportKind, extract_file, extract_regex};

    fn ts(lang: &str, src: &str) -> Extracted {
        extract(lang, false, src, true).expect("tree-sitter grammar for language")
    }

    fn defs_of(out: &Extracted) -> Vec<(&str, &str, u32)> {
        out.definitions.iter().map(|d| (d.name.as_str(), d.kind, d.line)).collect()
    }

    fn imports_of(out: &Extracted) -> Vec<(&str, ImportKind, u32)> {
        out.imports.iter().map(|i| (i.module.as_str(), i.kind, i.line)).collect()
    }

    #[test]
    fn every_covered_language_has_a_grammar() {
        for lang in ["rust", "python", "javascript", "typescript", "go", "java", "c", "cpp"] {
            assert!(extract(lang, false, "", true).is_some(), "no tree-sitter grammar for {lang}");
        }
        assert!(extract("markdown", false, "", true).is_none());
    }

    #[test]
    fn rust_multiline_use_and_mod_blocks() {
        let src = "use std::{\n    fmt,\n    io::{self, Read},\n};\npub mod inner {\n    pub struct Deep;\n    impl std::fmt::Display for Deep {\n        fn fmt(&self) {}\n    }\n}\nmod external;\n";
        let out = ts("rust", src);
        let imports = imports_of(&out);
        assert!(imports.contains(&("std", ImportKind::Path, 1)), "{imports:?}");
        assert!(imports.contains(&("external", ImportKind::ModDecl, 11)), "{imports:?}");
        let defs = defs_of(&out);
        assert!(defs.contains(&("inner", "mod", 5)), "{defs:?}");
        assert!(defs.contains(&("Deep", "struct", 6)), "{defs:?}");
        assert!(defs.contains(&("std::fmt::Display for Deep", "impl", 7)), "{defs:?}");
        assert!(defs.contains(&("fmt", "fn", 8)), "{defs:?}");
    }

    #[test]
    fn python_parenthesised_and_relative_imports() {
        let src = "from pkg.sub import (\n    alpha,\n    beta,\n)\nimport os, json as j\nfrom . import sibling\nfrom ..core import (thing)\nclass Runner:\n    def go(self):\n        pass\n";
        let out = ts("python", src);
        let imports = imports_of(&out);
        assert_eq!(
            imports,
            vec![
                ("pkg.sub", ImportKind::Path, 1),
                ("os", ImportKind::Path, 5),
                ("json", ImportKind::Path, 5),
                ("..core", ImportKind::Path, 7)
            ]
        );
        assert_eq!(defs_of(&out), vec![("Runner", "class", 8), ("go", "fn", 9)]);
    }

    #[test]
    fn typescript_type_imports_reexports_and_requires() {
        let src = "import type { Props } from \"./props\";\nimport React, { useState } from 'react';\nexport { Button } from \"./button\";\nconst lazy = await import(\"./lazy\");\nconst fs = require(\"fs\");\nexport interface Config { debug: boolean }\nexport type Id = string;\nexport const handler = async (req: Request) => req;\n";
        let out = ts("typescript", src);
        let imports = imports_of(&out);
        for expected in [("./props", 1), ("react", 2), ("./button", 3), ("./lazy", 4), ("fs", 5)] {
            assert!(imports.contains(&(expected.0, ImportKind::Path, expected.1)), "{imports:?}");
        }
        let defs = defs_of(&out);
        assert!(defs.contains(&("Config", "interface", 6)), "{defs:?}");
        assert!(defs.contains(&("Id", "type", 7)), "{defs:?}");
        assert!(defs.contains(&("handler", "function", 8)), "{defs:?}");
    }

    #[test]
    fn tsx_files_use_the_tsx_grammar() {
        let out = extract_file("web/App.tsx", "typescript", "function App() {\n  return <div className=\"x\">hi</div>;\n}\n", true);
        assert!(defs_of(&out).contains(&("App", "function", 1)));
    }

    #[test]
    fn go_grouped_imports() {
        let src = "package main\n\nimport (\n\t\"fmt\"\n\tlog \"github.com/x/log\"\n\t. \"strings\"\n)\n\nimport \"os\"\n\nfunc (s *Store) Save() {}\ntype Store struct{}\n";
        let out = ts("go", src);
        assert_eq!(
            imports_of(&out),
            vec![
                ("fmt", ImportKind::Path, 4),
                ("github.com/x/log", ImportKind::Path, 5),
                ("strings", ImportKind::Path, 6),
                ("os", ImportKind::Path, 9)
            ]
        );
        assert_eq!(defs_of(&out), vec![("Save", "func", 11), ("Store", "type", 12)]);
    }

    #[test]
    fn cpp_templates_namespaces_and_includes() {
        let src = "#include <vector>\n#include \"local/util.hpp\"\nnamespace engine {\ntemplate <typename T>\nclass Buffer {\npublic:\n    void push(T value) {}\n};\ntemplate <typename T>\nT clamp(T v, T lo, T hi) { return v; }\nstruct Pixel { int r; };\nvoid Buffer<int>::reset() {}\n}\n";
        let out = ts("cpp", src);
        let imports = imports_of(&out);
        assert!(imports.contains(&("vector", ImportKind::Path, 1)), "{imports:?}");
        assert!(imports.contains(&("local/util.hpp", ImportKind::Relative, 2)), "{imports:?}");
        let defs = defs_of(&out);
        for expected in [
            ("engine", "namespace", 3),
            ("Buffer", "class", 5),
            ("push", "fn", 7),
            ("clamp", "fn", 10),
            ("Pixel", "struct", 11),
            ("reset", "fn", 12),
        ] {
            assert!(defs.contains(&expected), "{defs:?}");
        }
    }

    #[test]
    fn partial_tree_keeps_what_parsed() {
        let out = ts("rust", "fn ok() {}\nfn broken( {\n    let x = ;\n");
        assert!(defs_of(&out).contains(&("ok", "fn", 1)));
    }

    #[test]
    fn identifiers_are_code_tokens_not_comment_words() {
        let out = ts("rust", "// Commentary words\nfn run_all() { helper_call(); }\n");
        assert!(out.identifiers.contains("helper_call"));
        assert!(out.identifiers.contains("run_all"));
        assert!(!out.identifiers.contains("Commentary"));
    }

    #[test]
    fn tree_sitter_finds_every_regex_definition() {
        let samples: [(&str, &str); 8] = [
            (
                "rust",
                "use std::{\n    fmt,\n};\npub mod inner {\n    pub struct Deep;\n    impl std::fmt::Display for Deep {\n        fn fmt(&self) {}\n    }\n}\nmod external;\n",
            ),
            ("python", "from pkg.sub import (\n    alpha,\n)\nimport os, json as j\nclass Runner:\n    def go(self):\n        pass\n"),
            ("javascript", "function greet(name) {}\nclass Box {}\nconst add = (a, b) => a + b;\n"),
            (
                "typescript",
                "import type { Props } from \"./props\";\nexport interface Config { debug: boolean }\nexport type Id = string;\nexport const handler = async (req: Request) => req;\n",
            ),
            ("go", "package main\n\nimport (\n\t\"fmt\"\n)\n\nfunc (s *Store) Save() {}\ntype Store struct{}\n"),
            ("java", "import java.util.List;\npublic class Repo {\n    public static Repo create(String name) { return null; }\n}\n"),
            ("c", "int add(int a, int b) {\n    return a + b;\n}\nstruct Point {\n    int x;\n};\n"),
            ("cpp", "namespace engine {\nclass Buffer {\n};\nstruct Pixel { int r; };\n}\n"),
        ];
        for (lang, src) in samples {
            let regex = extract_regex(lang, src, true);
            let tree = ts(lang, src);
            for d in &regex.definitions {
                assert!(
                    tree.definitions.iter().any(|t| t.name == d.name && t.kind == d.kind && t.line == d.line),
                    "{lang}: tree-sitter misses regex definition {d:?}"
                );
            }
            assert!(!tree.definitions.is_empty(), "{lang}: no definitions found");
        }
    }
}
