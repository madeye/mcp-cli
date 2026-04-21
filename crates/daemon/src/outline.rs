//! Outline / symbol extraction on top of a parsed `tree-sitter::Tree`.
//!
//! Kept out of `handlers.rs` so the query-execution logic is testable
//! against an in-memory ParseCache without going through the RPC layer.

use protocol::{CodeOutlineEntry, RpcError};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor};

use crate::languages::Language;
use crate::parse_cache::ParsedFile;

/// Extract outline entries from a parsed file. Walks the language's
/// compiled outline query and emits one entry per `@def.<kind>` capture.
/// When `signatures` is true, each entry's `signature` field is populated
/// with the declaration header (see `extract_signature`).
pub fn outline(parsed: &ParsedFile, signatures: bool) -> Result<Vec<CodeOutlineEntry>, RpcError> {
    let query = Query::new(
        &parsed.language.ts_language(),
        parsed.language.outline_query(),
    )
    .map_err(|e| RpcError::new(-32040, format!("outline query: {e:?}")))?;

    let capture_meta: Vec<Option<CaptureMeta>> = query
        .capture_names()
        .iter()
        .map(|name| CaptureMeta::from_capture_name(name))
        .collect();

    let mut cursor = QueryCursor::new();
    let mut out: Vec<CodeOutlineEntry> = Vec::new();
    let source = parsed.source.as_slice();

    let mut matches = cursor.matches(&query, parsed.tree.root_node(), source);
    while let Some(m) = matches.next() {
        let mut def_node = None;
        let mut def_kind = None;
        let mut name_text = None;

        for cap in m.captures {
            let meta = match capture_meta[cap.index as usize].as_ref() {
                Some(m) => m,
                None => continue,
            };
            match meta {
                CaptureMeta::Def(kind) => {
                    def_node = Some(cap.node);
                    def_kind = Some(*kind);
                }
                CaptureMeta::Name => {
                    if let Ok(text) = cap.node.utf8_text(source) {
                        name_text = Some(text.to_string());
                    }
                }
            }
        }

        if let (Some(node), Some(kind), Some(name)) = (def_node, def_kind, name_text) {
            let start = node.start_position();
            let end = node.end_position();
            let signature = if signatures {
                Some(extract_signature(node, source))
            } else {
                None
            };
            out.push(CodeOutlineEntry {
                kind: kind.to_string(),
                name,
                start_byte: node.start_byte() as u32,
                end_byte: node.end_byte() as u32,
                start_line: (start.row as u32) + 1,
                end_line: (end.row as u32) + 1,
                signature,
            });
        }
    }

    // Queries with overlapping patterns (e.g. `decorated_definition` plus
    // raw `function_definition` in Python) can double-report the same
    // declaration. Dedupe by byte range.
    out.sort_by_key(|e| (e.start_byte, e.end_byte));
    out.dedup_by_key(|e| (e.start_byte, e.end_byte));
    Ok(out)
}

/// Flat symbol names: the `name` field of every outline entry, stably
/// de-duplicated in first-seen order.
pub fn symbols(parsed: &ParsedFile) -> Result<Vec<String>, RpcError> {
    let entries = outline(parsed, false)?;
    let mut names: Vec<String> = Vec::with_capacity(entries.len());
    for e in entries {
        if !names.iter().any(|n| n == &e.name) {
            names.push(e.name);
        }
    }
    Ok(names)
}

/// Node kinds that mark where a declaration's body starts. Covers all
/// seven grammars currently wired in `languages.rs`. If the outline
/// query ever captures a node whose body uses a kind not listed here,
/// the signature falls back to the first line — still useful, just less
/// precise.
const BODY_KINDS: &[&str] = &[
    "block",
    "statement_block",
    "compound_statement",
    "field_declaration_list",
    "ordered_field_declaration_list",
    "enum_variant_list",
    "enumerator_list",
    "declaration_list",
    "class_body",
    "interface_body",
    "object_type",
    "enum_body",
    "macro_rule",
];

/// The byte-offset of the "body" of a declaration, if it has one.
/// Tries the `body` field first (grammar-agnostic path), then walks
/// named descendants up to `DFS_DEPTH` levels looking for one of
/// `BODY_KINDS`. Handles the Go `type_declaration -> type_spec ->
/// struct_type -> field_declaration_list` shape without hard-coding
/// Go specifics.
fn find_body_start(node: Node<'_>) -> Option<usize> {
    const DFS_DEPTH: usize = 4;
    if let Some(body) = node.child_by_field_name("body") {
        return Some(body.start_byte());
    }
    fn dfs(n: Node<'_>, depth: usize) -> Option<usize> {
        if depth == 0 {
            return None;
        }
        let mut cursor = n.walk();
        for child in n.named_children(&mut cursor) {
            if BODY_KINDS.contains(&child.kind()) {
                return Some(child.start_byte());
            }
            if let Some(b) = dfs(child, depth - 1) {
                return Some(b);
            }
        }
        None
    }
    dfs(node, DFS_DEPTH)
}

/// Build a compact signature string for a declaration node. Takes the
/// source bytes from the node's start up to its body (or, for bodiless
/// declarations, the first line), then collapses all runs of
/// whitespace to a single space and trims. Invalid UTF-8 degrades to
/// an empty string rather than failing — outline entries are best-effort.
fn extract_signature(node: Node<'_>, source: &[u8]) -> String {
    let start = node.start_byte();
    let body = find_body_start(node);
    let end = match body {
        Some(b) if b > start => b,
        _ => node.end_byte(),
    };
    let slice = source.get(start..end).unwrap_or(&[]);
    let text = std::str::from_utf8(slice).unwrap_or("");
    // First line when no body was found — keeps multi-line constants
    // and Go struct aliases from dumping the entire body.
    let candidate = if body.is_none() {
        text.split('\n').next().unwrap_or("")
    } else {
        text
    };
    let mut out = candidate.split_whitespace().collect::<Vec<_>>().join(" ");
    // `macro_rule` / `field_declaration_list` starts one byte after the
    // opening `{`, so the signature carries a trailing `{` we don't
    // want. Drop trailing open delimiters — they're leftover body
    // markers, never meaningful closers.
    while out.ends_with('{') || out.ends_with('(') || out.ends_with('[') {
        out.pop();
        out.truncate(out.trim_end().len());
    }
    out
}

#[allow(dead_code)]
pub fn language_for(parsed: &ParsedFile) -> Language {
    parsed.language
}

enum CaptureMeta {
    /// `@def.<kind>` — this capture marks a whole declaration node.
    Def(&'static str),
    /// `@name` — this capture supplies the declaration's name.
    Name,
}

impl CaptureMeta {
    fn from_capture_name(name: &str) -> Option<Self> {
        if name == "name" {
            return Some(Self::Name);
        }
        let rest = name.strip_prefix("def.")?;
        // Intern the suffix so entries get a `'static str` without leaking.
        let kind: &'static str = match rest {
            "function" => "function",
            "method" => "method",
            "struct" => "struct",
            "enum" => "enum",
            "class" => "class",
            "trait" => "trait",
            "interface" => "interface",
            "module" => "module",
            "namespace" => "namespace",
            "type" => "type",
            "constant" => "constant",
            "variable" => "variable",
            "macro" => "macro",
            "impl" => "impl",
            "union" => "union",
            "field" => "field",
            _ => return None,
        };
        Some(Self::Def(kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_cache::ParseCache;
    use std::io::Write;

    fn parse(ext: &str, body: &str) -> ParsedFile {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(format!("src.{ext}"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.sync_all().unwrap();
        let cache = ParseCache::new(1);
        let parsed = cache
            .get_or_parse(&path)
            .unwrap()
            .expect("language detected");
        std::mem::forget(tmp);
        parsed
    }

    fn kinds_and_names(entries: &[CodeOutlineEntry]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|e| (e.kind.clone(), e.name.clone()))
            .collect()
    }

    #[test]
    fn rust_outline() {
        let src = r#"
fn alpha() {}
struct Beta { x: u32 }
enum Gamma { A, B }
trait Delta {}
mod epsilon {}
const ZETA: u32 = 1;
type Eta = u32;
macro_rules! theta { () => {} }
impl Beta {}
"#;
        let parsed = parse("rs", src);
        let entries = outline(&parsed, false).unwrap();
        let got = kinds_and_names(&entries);
        let want = [
            ("function", "alpha"),
            ("struct", "Beta"),
            ("enum", "Gamma"),
            ("trait", "Delta"),
            ("module", "epsilon"),
            ("constant", "ZETA"),
            ("type", "Eta"),
            ("macro", "theta"),
            ("impl", "Beta"),
        ];
        for (kind, name) in want {
            assert!(
                got.iter().any(|(k, n)| k == kind && n == name),
                "rust outline missing {kind} {name} in {got:?}"
            );
        }
    }

    #[test]
    fn python_outline_handles_decorators() {
        let src = r#"
def plain(): pass

class Holder:
    def method(self): pass

@decorate
def decorated(): pass

@decorate
class Decorated:
    pass
"#;
        let parsed = parse("py", src);
        let got = kinds_and_names(&outline(&parsed, false).unwrap());
        for want in [
            ("function", "plain"),
            ("class", "Holder"),
            ("function", "decorated"),
            ("class", "Decorated"),
        ] {
            assert!(
                got.iter().any(|(k, n)| k == want.0 && n == want.1),
                "python outline missing {want:?} in {got:?}"
            );
        }
    }

    #[test]
    fn c_outline() {
        let src = r#"
int func(int x) { return x; }
struct Point { int x; int y; };
enum Color { Red, Green };
typedef int MyInt;
#define MAX 10
"#;
        let parsed = parse("c", src);
        let got = kinds_and_names(&outline(&parsed, false).unwrap());
        for want in [
            ("function", "func"),
            ("struct", "Point"),
            ("enum", "Color"),
            ("type", "MyInt"),
            ("macro", "MAX"),
        ] {
            assert!(
                got.iter().any(|(k, n)| k == want.0 && n == want.1),
                "c outline missing {want:?} in {got:?}"
            );
        }
    }

    #[test]
    fn cpp_outline() {
        let src = r#"
namespace ns {
class Widget {
public:
  void method();
};
struct Point { int x; };
enum class Color { Red };
}
int free_fn() { return 0; }
"#;
        let parsed = parse("cpp", src);
        let got = kinds_and_names(&outline(&parsed, false).unwrap());
        for want in [
            ("namespace", "ns"),
            ("class", "Widget"),
            ("struct", "Point"),
            ("function", "free_fn"),
        ] {
            assert!(
                got.iter().any(|(k, n)| k == want.0 && n == want.1),
                "cpp outline missing {want:?} in {got:?}"
            );
        }
    }

    #[test]
    fn typescript_outline() {
        let src = r#"
function plain() {}
class Holder {
  method() {}
}
interface Shape { x: number }
type Alias = number;
enum Color { Red, Green }
"#;
        let parsed = parse("ts", src);
        let got = kinds_and_names(&outline(&parsed, false).unwrap());
        for want in [
            ("function", "plain"),
            ("class", "Holder"),
            ("interface", "Shape"),
            ("type", "Alias"),
            ("enum", "Color"),
        ] {
            assert!(
                got.iter().any(|(k, n)| k == want.0 && n == want.1),
                "typescript outline missing {want:?} in {got:?}"
            );
        }
    }

    #[test]
    fn go_outline() {
        let src = r#"
package main

func plain() {}

type Point struct { X, Y int }

func (p Point) method() {}

const Pi = 3.14

var Name = "go"
"#;
        let parsed = parse("go", src);
        let got = kinds_and_names(&outline(&parsed, false).unwrap());
        for want in [
            ("function", "plain"),
            ("type", "Point"),
            ("method", "method"),
            ("constant", "Pi"),
            ("variable", "Name"),
        ] {
            assert!(
                got.iter().any(|(k, n)| k == want.0 && n == want.1),
                "go outline missing {want:?} in {got:?}"
            );
        }
    }

    #[test]
    fn symbols_are_deduped_flat_names() {
        let src = "fn a() {} fn a() {} struct B;";
        let parsed = parse("rs", src);
        let names = symbols(&parsed).unwrap();
        assert_eq!(names, vec!["a".to_string(), "B".to_string()]);
    }

    #[test]
    fn byte_ranges_cover_declaration() {
        let src = "fn alpha() {}\n";
        let parsed = parse("rs", src);
        let entries = outline(&parsed, false).unwrap();
        let alpha = entries.iter().find(|e| e.name == "alpha").unwrap();
        assert_eq!(alpha.start_byte, 0);
        // Ends at the closing brace.
        assert_eq!(alpha.end_byte as usize, src.trim_end().len());
        assert_eq!(alpha.start_line, 1);
    }

    #[test]
    fn signatures_absent_by_default() {
        let parsed = parse("rs", "fn alpha() {}\n");
        let entries = outline(&parsed, false).unwrap();
        assert!(entries.iter().all(|e| e.signature.is_none()));
    }

    fn sig_by_name<'a>(entries: &'a [CodeOutlineEntry], name: &str) -> &'a str {
        entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("no entry named {name} in {entries:?}"))
            .signature
            .as_deref()
            .unwrap_or_else(|| panic!("entry {name} has no signature"))
    }

    #[test]
    fn rust_signatures_strip_bodies_and_normalize() {
        let src = r#"
fn alpha(x: u32,
         y: u32) -> u32 { x + y }
struct Beta { x: u32, y: u32 }
struct Unit;
enum Gamma { A, B }
trait Delta { fn m(); }
mod epsilon { fn inner() {} }
const ZETA: u32 = 1;
type Eta = u32;
macro_rules! theta { () => {} }
impl<T> Beta where T: Clone {}
"#;
        let parsed = parse("rs", src);
        let entries = outline(&parsed, true).unwrap();
        assert_eq!(
            sig_by_name(&entries, "alpha"),
            "fn alpha(x: u32, y: u32) -> u32"
        );
        assert_eq!(sig_by_name(&entries, "Beta"), "struct Beta");
        assert_eq!(sig_by_name(&entries, "Unit"), "struct Unit;");
        assert_eq!(sig_by_name(&entries, "Gamma"), "enum Gamma");
        assert_eq!(sig_by_name(&entries, "Delta"), "trait Delta");
        assert_eq!(sig_by_name(&entries, "epsilon"), "mod epsilon");
        assert_eq!(sig_by_name(&entries, "ZETA"), "const ZETA: u32 = 1;");
        assert_eq!(sig_by_name(&entries, "Eta"), "type Eta = u32;");
        assert_eq!(sig_by_name(&entries, "theta"), "macro_rules! theta");
        assert_eq!(sig_by_name(&entries, "Beta"), "struct Beta");
        // `impl` captured by name `Beta` (the target type).
        let impl_sig = entries
            .iter()
            .find(|e| e.kind == "impl")
            .unwrap()
            .signature
            .as_deref()
            .unwrap();
        assert_eq!(impl_sig, "impl<T> Beta where T: Clone");
    }

    #[test]
    fn python_signatures_drop_bodies() {
        let src = r#"
def plain(x,
          y):
    return x + y

class Holder:
    def method(self): pass
"#;
        let parsed = parse("py", src);
        let entries = outline(&parsed, true).unwrap();
        assert_eq!(sig_by_name(&entries, "plain"), "def plain(x, y):");
        assert_eq!(sig_by_name(&entries, "Holder"), "class Holder:");
        assert_eq!(sig_by_name(&entries, "method"), "def method(self):");
    }

    #[test]
    fn typescript_signatures_drop_bodies() {
        let src = r#"
function plain(x: number, y: number): number { return x + y }
class Holder {
  method(a: string): void {}
}
interface Shape { x: number }
type Alias = number;
enum Color { Red, Green }
"#;
        let parsed = parse("ts", src);
        let entries = outline(&parsed, true).unwrap();
        assert_eq!(
            sig_by_name(&entries, "plain"),
            "function plain(x: number, y: number): number"
        );
        assert_eq!(sig_by_name(&entries, "Holder"), "class Holder");
        assert_eq!(sig_by_name(&entries, "method"), "method(a: string): void");
        assert_eq!(sig_by_name(&entries, "Shape"), "interface Shape");
        assert_eq!(sig_by_name(&entries, "Alias"), "type Alias = number;");
        assert_eq!(sig_by_name(&entries, "Color"), "enum Color");
    }

    #[test]
    fn go_signatures_drop_bodies() {
        let src = r#"
package main

func plain(x int, y int) int { return x + y }

type Point struct { X, Y int }

func (p Point) method() int { return 0 }

const Pi = 3.14

var Name = "go"
"#;
        let parsed = parse("go", src);
        let entries = outline(&parsed, true).unwrap();
        assert_eq!(
            sig_by_name(&entries, "plain"),
            "func plain(x int, y int) int"
        );
        assert_eq!(sig_by_name(&entries, "Point"), "type Point struct");
        assert_eq!(
            sig_by_name(&entries, "method"),
            "func (p Point) method() int"
        );
        assert_eq!(sig_by_name(&entries, "Pi"), "const Pi = 3.14");
        assert_eq!(sig_by_name(&entries, "Name"), "var Name = \"go\"");
    }

    #[test]
    fn c_signatures_drop_bodies() {
        let src = r#"
int func(int x,
         int y) { return x + y; }
struct Point { int x; int y; };
enum Color { Red, Green };
typedef int MyInt;
#define MAX 10
"#;
        let parsed = parse("c", src);
        let entries = outline(&parsed, true).unwrap();
        assert_eq!(sig_by_name(&entries, "func"), "int func(int x, int y)");
        assert_eq!(sig_by_name(&entries, "Point"), "struct Point");
        assert_eq!(sig_by_name(&entries, "Color"), "enum Color");
        assert_eq!(sig_by_name(&entries, "MyInt"), "typedef int MyInt;");
    }
}
