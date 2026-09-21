//! Module resolution: an `oxc_resolver` with a memoised specifier cache.
//!
//! Two resolvers, because a stylesheet is not resolved the way a module is. Sass has
//! its own rules — see [`Resolver::resolve`] — and running them through the
//! JavaScript resolver would find nothing.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use ahash::AHashMap;
use oxc_resolver::{
    PackageJson, Resolution, ResolveOptions, Resolver as OxcResolver, SideEffects as Declared,
    TsconfigDiscovery,
};

use crate::module::is_style_file;

/// Modules Sass ships with.
///
/// `@use "sass:math"` names no file, and nothing on disk could answer it — a `:` is
/// not legal in a package name, so the lookup this skips would fail anyway. Listing
/// them changes no verdict. It is here to say what these are, so that a specifier
/// resolving to nothing is not read later as a gap to be closed.
const SASS_BUILTINS: &[&str] = &[
    "sass:math",
    "sass:color",
    "sass:list",
    "sass:map",
    "sass:meta",
    "sass:selector",
    "sass:string",
];

/// What importing a file can do beyond defining its exports.
///
/// This is the `sideEffects` field of the nearest `package.json`: a claim the author
/// makes to bundlers, which we read the way they read it and trust the way they trust
/// it. A wrong claim already breaks the build it ships in.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SideEffects {
    /// Importing may run something. The answer whenever the field is absent, says so,
    /// or is written in a way we cannot read.
    Possible,
    /// The author declares that importing only defines bindings.
    None,
}

/// Resolves import specifiers to absolute paths, caching every answer
/// (including failures) per `(importing file, specifier)` pair.
pub struct Resolver {
    inner: OxcResolver,
    /// The same, tuned for Sass. Used when the importing file is a stylesheet.
    style: OxcResolver,
    cache: RwLock<AHashMap<(PathBuf, String), Option<PathBuf>>>,
    /// The `sideEffects` verdict for each path this resolver has produced, recorded
    /// while the resolution that found its `package.json` is still in hand.
    side_effects: RwLock<AHashMap<PathBuf, SideEffects>>,
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

        // Sass looks for `_name.scss` beside `name.scss`, takes `_index.scss` for a
        // directory, and tries the importing file's own directory before anything
        // else — so a bare `@use "mixins"` is usually a sibling rather than a
        // package. The `exports` field is left out because Sass tooling resolves a
        // subpath by path, and honouring it would refuse targets that do resolve.
        let style_options = ResolveOptions {
            extensions: vec![".scss".to_string(), ".css".to_string()],
            main_files: vec!["_index".to_string(), "index".to_string()],
            exports_fields: Vec::new(),
            prefer_relative: true,
            ..ResolveOptions::default()
        };

        Self {
            inner: OxcResolver::new(options),
            style: OxcResolver::new(style_options),
            cache: RwLock::new(AHashMap::default()),
            side_effects: RwLock::new(AHashMap::default()),
        }
    }

    /// Resolves `specifier` as written in `from_file`, or `None` if it does not
    /// point at a file on disk.
    pub fn resolve(&self, from_file: &Path, specifier: &str) -> Option<PathBuf> {
        let cache_key = (from_file.to_path_buf(), specifier.to_string());
        if let Some(cached) = self.cache.read().unwrap().get(&cache_key) {
            return cached.clone();
        }

        let resolution = if is_style_file(from_file) {
            self.resolve_style(from_file, specifier)
        } else {
            let mut resolution = self.inner.resolve_file(from_file, specifier).ok();
            if resolution.is_none()
                && let Some(request) = strip_inline_loaders(specifier)
            {
                resolution = self.inner.resolve_file(from_file, request).ok();
            }
            resolution
        };

        let result = resolution.map(|resolution| {
            let path = resolution.path().to_path_buf();
            self.side_effects
                .write()
                .unwrap()
                .insert(path.clone(), declared_side_effects(&resolution));
            path
        });

        self.cache
            .write()
            .unwrap()
            .insert(cache_key, result.clone());
        result
    }

    /// Resolves `specifier` the way Sass would.
    ///
    /// Beyond what the tuned resolver already does, two rules are applied here
    /// because they are about the specifier rather than about the search:
    ///
    /// - A leading `~` is dropped. It is a bundler convention meaning "not
    ///   relative", and what follows it is an ordinary request.
    /// - A partial is tried. Sass keeps a file meant only for importing under a
    ///   leading underscore and lets it be named without one, so `a/b` is also
    ///   `a/_b`.
    ///
    /// A `sass:` module resolves to nothing, and should: it names no file.
    fn resolve_style(&self, from_file: &Path, specifier: &str) -> Option<Resolution> {
        if SASS_BUILTINS.contains(&specifier) {
            return None;
        }
        let request = specifier.strip_prefix('~').unwrap_or(specifier);
        let partial = match request.rsplit_once('/') {
            Some((dir, base)) if !base.starts_with('_') => Some(format!("{dir}/_{base}")),
            None if !request.starts_with('_') => Some(format!("_{request}")),
            _ => None,
        };

        std::iter::once(request.to_string())
            .chain(partial)
            .find_map(|candidate| self.style.resolve_file(from_file, &candidate).ok())
    }

    /// What the nearest `package.json` says about importing `path`.
    ///
    /// `Possible` for a path this resolver never produced: a file we have not placed
    /// in a package is never assumed inert.
    pub fn side_effects(&self, path: &Path) -> SideEffects {
        self.side_effects
            .read()
            .unwrap()
            .get(path)
            .copied()
            .unwrap_or(SideEffects::Possible)
    }
}

fn declared_side_effects(resolution: &Resolution) -> SideEffects {
    let Some(package) = resolution.package_json() else {
        return SideEffects::Possible;
    };
    match package.side_effects() {
        None | Some(Declared::Bool(true)) => SideEffects::Possible,
        Some(Declared::Bool(false)) => SideEffects::None,
        Some(Declared::String(pattern)) => match_globs(package, resolution.path(), &[pattern]),
        Some(Declared::Array(patterns)) => match_globs(package, resolution.path(), &patterns),
    }
}

/// A glob list names the files that *do* have side effects; everything else is inert.
///
/// Anything that stops the path or a pattern from being read leaves the file as
/// `Possible`, because the alternative is dropping an edge on a guess.
fn match_globs(package: &PackageJson, path: &Path, patterns: &[&str]) -> SideEffects {
    let Some(relative) = path
        .strip_prefix(package.directory())
        .ok()
        .and_then(|relative| relative.to_str())
    else {
        return SideEffects::Possible;
    };
    let relative = relative.replace('\\', "/");

    for pattern in patterns {
        // `glob_match` has no answer for a malformed pattern, and its non-answer
        // reads as "no match" — the one direction we cannot afford to be wrong in.
        if fast_glob::validate(pattern).is_err() || matches(pattern, &relative) {
            return SideEffects::Possible;
        }
    }
    SideEffects::None
}

/// Webpack's reading of a `sideEffects` glob: matched against the path relative to
/// the package root, and a pattern that names no directory matches at any depth.
fn matches(pattern: &str, relative: &str) -> bool {
    let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
    if pattern.contains('/') {
        fast_glob::glob_match(pattern, relative)
    } else {
        fast_glob::glob_match(format!("**/{pattern}"), relative)
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
    fn a_bare_pattern_matches_at_any_depth() {
        // Webpack prefixes `**/` to a pattern that names no directory, which is what
        // makes the common `"*.css"` cover a stylesheet anywhere in the package.
        assert!(matches("*.css", "src/styles/theme.css"));
        assert!(matches("*.css", "theme.css"));
        assert!(!matches("*.css", "src/styles/theme.scss"));
    }

    #[test]
    fn a_pattern_naming_a_directory_is_anchored_at_the_package_root() {
        assert!(matches("src/*.ts", "src/index.ts"));
        assert!(!matches("src/*.ts", "index.ts"));
        // A single star does not cross a separator; only `**` does.
        assert!(!matches("src/*.ts", "src/nested/a.ts"));
        assert!(matches("src/**/*.ts", "src/nested/a.ts"));
    }

    #[test]
    fn a_leading_dot_slash_is_not_part_of_the_pattern() {
        assert!(matches("./src/index.js", "src/index.js"));
    }

    #[test]
    fn a_malformed_pattern_is_rejected_rather_than_matched() {
        // `glob_match` would answer "no match", which would silently drop an edge.
        assert!(fast_glob::validate("src/{polyfills.ts").is_err());
        assert!(fast_glob::validate("src/**/*.css").is_ok());
    }

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
