//! Language detection and per-language tree-sitter queries.
//!
//! Adding a language means wiring four things: a variant in `Language`,
//! a case in `detect`, its `tree_sitter::Language` constructor, and the
//! outline/symbols query strings. Keep the queries minimal — they run
//! on every `code.outline` call against every parsed file.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    C,
    Cpp,
    TypeScript,
    Tsx,
    Go,
}

impl Language {
    pub fn detect(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Self::Rust,
            "py" | "pyi" => Self::Python,
            "c" | "h" => Self::C,
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Self::Cpp,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "go" => Self::Go,
            _ => return None,
        })
    }

    pub fn ts_language(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::Go => "go",
        }
    }

    /// Outline query: every `@def` capture becomes an outline entry; the
    /// adjacent `@name` capture (same match) supplies its name; the
    /// optional `@kind` capture (or a default based on the capture's
    /// group in the query) supplies the kind label.
    pub fn outline_query(self) -> &'static str {
        match self {
            Self::Rust => RUST_OUTLINE,
            Self::Python => PYTHON_OUTLINE,
            Self::C => C_OUTLINE,
            Self::Cpp => CPP_OUTLINE,
            Self::TypeScript | Self::Tsx => TS_OUTLINE,
            Self::Go => GO_OUTLINE,
        }
    }
}

// ---- Outline queries --------------------------------------------------------

const RUST_OUTLINE: &str = r#"
(function_item name: (identifier) @name) @def.function
(struct_item name: (type_identifier) @name) @def.struct
(enum_item name: (type_identifier) @name) @def.enum
(trait_item name: (type_identifier) @name) @def.trait
(mod_item name: (identifier) @name) @def.module
(type_item name: (type_identifier) @name) @def.type
(const_item name: (identifier) @name) @def.constant
(static_item name: (identifier) @name) @def.constant
(macro_definition name: (identifier) @name) @def.macro
(impl_item type: (type_identifier) @name) @def.impl
"#;

const PYTHON_OUTLINE: &str = r#"
(function_definition name: (identifier) @name) @def.function
(class_definition name: (identifier) @name) @def.class
(decorated_definition
  definition: (function_definition name: (identifier) @name)) @def.function
(decorated_definition
  definition: (class_definition name: (identifier) @name)) @def.class
"#;

const C_OUTLINE: &str = r#"
(function_definition declarator: (function_declarator declarator: (identifier) @name)) @def.function
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name))) @def.function
(struct_specifier name: (type_identifier) @name) @def.struct
(enum_specifier name: (type_identifier) @name) @def.enum
(union_specifier name: (type_identifier) @name) @def.union
(type_definition declarator: (type_identifier) @name) @def.type
(preproc_def name: (identifier) @name) @def.macro
(preproc_function_def name: (identifier) @name) @def.macro
"#;

const CPP_OUTLINE: &str = r#"
(function_definition declarator: (function_declarator declarator: (identifier) @name)) @def.function
(function_definition declarator: (function_declarator declarator: (field_identifier) @name)) @def.method
(function_definition declarator: (function_declarator declarator: (qualified_identifier) @name)) @def.method
(class_specifier name: (type_identifier) @name) @def.class
(struct_specifier name: (type_identifier) @name) @def.struct
(enum_specifier name: (type_identifier) @name) @def.enum
(union_specifier name: (type_identifier) @name) @def.union
(namespace_definition name: (namespace_identifier) @name) @def.namespace
(type_definition declarator: (type_identifier) @name) @def.type
(alias_declaration name: (type_identifier) @name) @def.type
(preproc_def name: (identifier) @name) @def.macro
(preproc_function_def name: (identifier) @name) @def.macro
"#;

const TS_OUTLINE: &str = r#"
(function_declaration name: (identifier) @name) @def.function
(class_declaration name: (type_identifier) @name) @def.class
(interface_declaration name: (type_identifier) @name) @def.interface
(type_alias_declaration name: (type_identifier) @name) @def.type
(enum_declaration name: (identifier) @name) @def.enum
(method_definition name: (property_identifier) @name) @def.method
(public_field_definition name: (property_identifier) @name) @def.field
"#;

const GO_OUTLINE: &str = r#"
(function_declaration name: (identifier) @name) @def.function
(method_declaration name: (field_identifier) @name) @def.method
(type_declaration (type_spec name: (type_identifier) @name)) @def.type
(const_declaration (const_spec name: (identifier) @name)) @def.constant
(var_declaration (var_spec name: (identifier) @name)) @def.variable
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detect_by_extension() {
        assert_eq!(
            Language::detect(&PathBuf::from("a.rs")),
            Some(Language::Rust)
        );
        assert_eq!(
            Language::detect(&PathBuf::from("a.py")),
            Some(Language::Python)
        );
        assert_eq!(
            Language::detect(&PathBuf::from("a.pyi")),
            Some(Language::Python)
        );
        assert_eq!(Language::detect(&PathBuf::from("a.c")), Some(Language::C));
        assert_eq!(Language::detect(&PathBuf::from("a.h")), Some(Language::C));
        assert_eq!(
            Language::detect(&PathBuf::from("a.cpp")),
            Some(Language::Cpp)
        );
        assert_eq!(
            Language::detect(&PathBuf::from("a.hh")),
            Some(Language::Cpp)
        );
        assert_eq!(
            Language::detect(&PathBuf::from("a.ts")),
            Some(Language::TypeScript)
        );
        assert_eq!(
            Language::detect(&PathBuf::from("a.tsx")),
            Some(Language::Tsx)
        );
        assert_eq!(Language::detect(&PathBuf::from("a.go")), Some(Language::Go));
        assert_eq!(Language::detect(&PathBuf::from("README.md")), None);
        assert_eq!(Language::detect(&PathBuf::from("noext")), None);
    }

    #[test]
    fn outline_queries_parse_cleanly() {
        // Every outline query must be valid tree-sitter syntax against its
        // language; otherwise handlers blow up at the first request instead
        // of at startup. This test catches query typos before users do.
        for lang in [
            Language::Rust,
            Language::Python,
            Language::C,
            Language::Cpp,
            Language::TypeScript,
            Language::Tsx,
            Language::Go,
        ] {
            let ts = lang.ts_language();
            tree_sitter::Query::new(&ts, lang.outline_query())
                .unwrap_or_else(|e| panic!("{} outline query: {e:?}", lang.name()));
        }
    }
}
