//! Module resolution: an `oxc_resolver` with a memoised specifier cache.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use ahash::AHashMap;
use oxc_resolver::{ResolveOptions, Resolver as OxcResolver, TsconfigDiscovery};

/// Resolves import specifiers to absolute paths, caching every answer
/// (including failures) per `(importing file, specifier)` pair.
pub struct Resolver {
    inner: OxcResolver,
    cache: RwLock<AHashMap<(PathBuf, String), Option<PathBuf>>>,
}

impl Resolver {
    pub fn new() -> Self {
        let options = ResolveOptions {
            extensions: vec![
                ".tsx".to_string(),
                ".ts".to_string(),
                ".cts".to_string(),
                ".mts".to_string(),
                ".jsx".to_string(),
                ".js".to_string(),
                ".mjs".to_string(),
                ".cjs".to_string(),
                ".json".to_string(),
            ],
            tsconfig: Some(TsconfigDiscovery::Auto),
            ..ResolveOptions::default()
        };

        Self {
            inner: OxcResolver::new(options),
            cache: RwLock::new(AHashMap::default()),
        }
    }

    /// Resolves `specifier` as written in `from_file`, or `None` if it does not
    /// point at a file on disk.
    pub fn resolve(&self, from_file: &Path, specifier: &str) -> Option<PathBuf> {
        let cache_key = (from_file.to_path_buf(), specifier.to_string());
        if let Some(cached) = self.cache.read().unwrap().get(&cache_key) {
            return cached.clone();
        }

        let mut result = self
            .inner
            .resolve_file(from_file, specifier)
            .ok()
            .map(|r| r.into_path_buf());
        if result.is_none() {
            if let Some(request) = strip_inline_loaders(specifier) {
                result = self
                    .inner
                    .resolve_file(from_file, request)
                    .ok()
                    .map(|r| r.into_path_buf());
            }
        }

        self.cache
            .write()
            .unwrap()
            .insert(cache_key, result.clone());
        result
    }
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new()
    }
}

/// Webpack lets an import name its loaders inline, as in `!!file-loader!./logo.png`.
/// Only the trailing request is a path. Returns `None` when there is nothing to strip.
///
/// A `?query` or `#fragment` suffix needs no such treatment: the resolver parses those
/// itself, which is why the resolved path is read back without them.
fn strip_inline_loaders(specifier: &str) -> Option<&str> {
    if !specifier.contains('!') {
        return None;
    }

    let request = specifier.rsplit('!').next().unwrap_or(specifier);
    if request.is_empty() || request == specifier {
        None
    } else {
        Some(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_webpack_inline_loaders() {
        assert_eq!(
            strip_inline_loaders("!!file-loader!./logo.png"),
            Some("./logo.png")
        );
        assert_eq!(
            strip_inline_loaders("style-loader!css-loader!./a.css"),
            Some("./a.css")
        );
    }

    #[test]
    fn leaves_ordinary_specifiers_alone() {
        assert_eq!(strip_inline_loaders("./logo.png"), None);
        assert_eq!(strip_inline_loaders("react"), None);
        // The resolver handles queries itself, so they are not our business.
        assert_eq!(strip_inline_loaders("./logo.png?url"), None);
    }

    #[test]
    fn rejects_a_specifier_that_is_only_loaders() {
        assert_eq!(strip_inline_loaders("!!file-loader!"), None);
    }
}
