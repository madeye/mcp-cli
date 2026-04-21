//! Tree-sitter backend: the default generalist.
//!
//! Wraps the per-file `ParseCache` + outline-query machinery already
//! living in `crate::outline`. This backend claims every `Language`
//! variant — register it last so specialist backends (rust-analyzer,
//! clangd) get first refusal on the languages they cover.

use std::path::Path;
use std::sync::Arc;

use protocol::RpcError;

use crate::languages::Language;
use crate::outline as ts_outline;
use crate::parse_cache::{ParseCache, ParsedFile};

use super::{LanguageBackend, OutlineResult, SymbolsResult};

pub struct TreeSitterBackend {
    parse_cache: Arc<ParseCache>,
}

impl TreeSitterBackend {
    pub fn new(parse_cache: Arc<ParseCache>) -> Self {
        Self { parse_cache }
    }

    fn parse(&self, path: &Path) -> Result<Option<ParsedFile>, RpcError> {
        self.parse_cache
            .get_or_parse(path)
            .map_err(|e| RpcError::new(-32041, format!("parse {}: {e}", path.display())))
    }
}

impl LanguageBackend for TreeSitterBackend {
    fn name(&self) -> &'static str {
        "tree-sitter"
    }

    fn supports(&self, _language: Language) -> bool {
        // Every language enum variant has a tree-sitter grammar wired
        // up in `languages.rs`. If a variant is added without a grammar,
        // `ParseCache::get_or_parse` will surface the failure.
        true
    }

    fn outline(
        &self,
        path: &Path,
        language: Language,
        signatures: bool,
    ) -> Result<OutlineResult, RpcError> {
        let parsed = match self.parse(path)? {
            Some(p) => p,
            None => {
                return Ok(OutlineResult {
                    language,
                    entries: Vec::new(),
                });
            }
        };
        let entries = ts_outline::outline(&parsed, signatures)?;
        Ok(OutlineResult {
            language: parsed.language,
            entries,
        })
    }

    fn symbols(&self, path: &Path, language: Language) -> Result<SymbolsResult, RpcError> {
        let parsed = match self.parse(path)? {
            Some(p) => p,
            None => {
                return Ok(SymbolsResult {
                    language,
                    names: Vec::new(),
                });
            }
        };
        let names = ts_outline::symbols(&parsed)?;
        Ok(SymbolsResult {
            language: parsed.language,
            names,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, body: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.sync_all().unwrap();
    }

    #[test]
    fn outline_via_backend_matches_direct_call() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.rs");
        write(&path, "fn alpha() {}\nstruct Beta;\n");
        let cache = Arc::new(ParseCache::new(4));
        let backend = TreeSitterBackend::new(cache);

        let result = backend.outline(&path, Language::Rust, false).unwrap();
        assert_eq!(result.language, Language::Rust);
        let names: Vec<_> = result.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"Beta"));
        // Signatures are opt-in.
        assert!(result.entries.iter().all(|e| e.signature.is_none()));
    }

    #[test]
    fn outline_via_backend_populates_signatures_when_requested() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.rs");
        write(&path, "fn alpha(x: u32) -> u32 { x + 1 }\n");
        let cache = Arc::new(ParseCache::new(4));
        let backend = TreeSitterBackend::new(cache);

        let result = backend.outline(&path, Language::Rust, true).unwrap();
        let alpha = result.entries.iter().find(|e| e.name == "alpha").unwrap();
        assert_eq!(alpha.signature.as_deref(), Some("fn alpha(x: u32) -> u32"));
    }

    #[test]
    fn symbols_via_backend_dedupes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.rs");
        write(&path, "fn a() {} fn a() {} struct B;");
        let cache = Arc::new(ParseCache::new(4));
        let backend = TreeSitterBackend::new(cache);

        let result = backend.symbols(&path, Language::Rust).unwrap();
        assert_eq!(result.names, vec!["a".to_string(), "B".to_string()]);
    }
}
