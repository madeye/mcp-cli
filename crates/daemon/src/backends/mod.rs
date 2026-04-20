//! Pluggable language backends.
//!
//! A `LanguageBackend` answers structural questions about a source file
//! (`outline`, `symbols`, eventually `definition`/`references`/`diagnostics`)
//! for a fixed set of languages. The daemon owns a `BackendRegistry` and
//! routes each request to the first registered backend that claims the
//! file's language.
//!
//! The point of the trait is to let M3+ plug in heavier backends
//! (rust-analyzer over LSP, clangd) without touching the RPC handlers.
//! For now the only implementation is `TreeSitterBackend`, which wraps
//! the existing per-file `ParseCache` + outline-query machinery.

use std::path::Path;
use std::sync::Arc;

use protocol::{CodeOutlineEntry, RpcError};

use crate::languages::Language;

pub mod tree_sitter;

pub use self::tree_sitter::TreeSitterBackend;

/// Outline result with the resolving language attached so callers can
/// surface it to the client without re-detecting from the path.
pub struct OutlineResult {
    pub language: Language,
    pub entries: Vec<CodeOutlineEntry>,
}

/// Flat de-duplicated symbol list for a file.
pub struct SymbolsResult {
    pub language: Language,
    pub names: Vec<String>,
}

/// What a backend has to provide. Methods receive the already-detected
/// `Language` so backends don't have to redo extension matching.
pub trait LanguageBackend: Send + Sync {
    /// Stable identifier used in logs / diagnostics. e.g. `tree-sitter`,
    /// `rust-analyzer`, `clangd`.
    fn name(&self) -> &'static str;

    /// Return true iff this backend can answer queries for `language`.
    /// The registry consults this in registration order; the first match
    /// wins, so register specialist backends (rust-analyzer) before
    /// generalist ones (tree-sitter).
    fn supports(&self, language: Language) -> bool;

    fn outline(&self, path: &Path, language: Language) -> Result<OutlineResult, RpcError>;
    fn symbols(&self, path: &Path, language: Language) -> Result<SymbolsResult, RpcError>;
}

/// Ordered list of backends. The daemon constructs this once at startup
/// and shares it across all connections; backends are expected to be
/// internally synchronized (the trait is `Send + Sync`).
pub struct BackendRegistry {
    backends: Vec<Arc<dyn LanguageBackend>>,
}

impl BackendRegistry {
    pub fn new() -> Self {
        Self {
            backends: Vec::new(),
        }
    }

    pub fn register(&mut self, backend: Arc<dyn LanguageBackend>) {
        tracing::info!(backend = backend.name(), "registered language backend");
        self.backends.push(backend);
    }

    /// First backend that claims `language`, or `None` if no backend handles it.
    pub fn for_language(&self, language: Language) -> Option<&Arc<dyn LanguageBackend>> {
        self.backends.iter().find(|b| b.supports(language))
    }

    /// Resolve the file's language and dispatch `outline`. Returns
    /// `Ok(None)` when the extension is unknown or no backend handles it
    /// — handlers map that to an empty result, not an error.
    pub fn outline(&self, path: &Path) -> Result<Option<OutlineResult>, RpcError> {
        let Some(language) = Language::detect(path) else {
            return Ok(None);
        };
        let Some(backend) = self.for_language(language) else {
            return Ok(None);
        };
        backend.outline(path, language).map(Some)
    }

    pub fn symbols(&self, path: &Path) -> Result<Option<SymbolsResult>, RpcError> {
        let Some(language) = Language::detect(path) else {
            return Ok(None);
        };
        let Some(backend) = self.for_language(language) else {
            return Ok(None);
        };
        backend.symbols(path, language).map(Some)
    }
}

impl Default for BackendRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingBackend {
        languages: Vec<Language>,
        outline_calls: AtomicUsize,
    }

    impl CountingBackend {
        fn new(languages: Vec<Language>) -> Self {
            Self {
                languages,
                outline_calls: AtomicUsize::new(0),
            }
        }
    }

    impl LanguageBackend for CountingBackend {
        fn name(&self) -> &'static str {
            "counting"
        }
        fn supports(&self, language: Language) -> bool {
            self.languages.contains(&language)
        }
        fn outline(&self, _path: &Path, language: Language) -> Result<OutlineResult, RpcError> {
            self.outline_calls.fetch_add(1, Ordering::SeqCst);
            Ok(OutlineResult {
                language,
                entries: Vec::new(),
            })
        }
        fn symbols(&self, _path: &Path, language: Language) -> Result<SymbolsResult, RpcError> {
            Ok(SymbolsResult {
                language,
                names: Vec::new(),
            })
        }
    }

    #[test]
    fn registry_routes_to_first_supporting_backend() {
        let rust_only = Arc::new(CountingBackend::new(vec![Language::Rust]));
        let everything = Arc::new(CountingBackend::new(vec![Language::Rust, Language::Python]));
        let mut reg = BackendRegistry::new();
        reg.register(rust_only.clone());
        reg.register(everything.clone());
        assert_eq!(reg.backends.len(), 2);

        // Rust hits the specialist first, not the generalist.
        let path = PathBuf::from("a.rs");
        let _ = reg.outline(&path).unwrap().expect("rust handled");
        assert_eq!(rust_only.outline_calls.load(Ordering::SeqCst), 1);
        assert_eq!(everything.outline_calls.load(Ordering::SeqCst), 0);

        // Python falls through to the generalist.
        let py = PathBuf::from("a.py");
        let _ = reg.outline(&py).unwrap().expect("python handled");
        assert_eq!(everything.outline_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn backend_name_is_exposed() {
        let b = Arc::new(CountingBackend::new(vec![Language::Rust]));
        assert_eq!(b.name(), "counting");
    }

    #[test]
    fn registry_returns_none_for_unknown_extension() {
        let mut reg = BackendRegistry::new();
        reg.register(Arc::new(CountingBackend::new(vec![Language::Rust])));
        assert!(reg.outline(&PathBuf::from("README.md")).unwrap().is_none());
    }

    #[test]
    fn registry_returns_none_when_no_backend_handles_language() {
        let mut reg = BackendRegistry::new();
        reg.register(Arc::new(CountingBackend::new(vec![Language::Python])));
        // Rust is detected but no backend claims it.
        assert!(reg.outline(&PathBuf::from("a.rs")).unwrap().is_none());
    }
}
