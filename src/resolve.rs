//! Module resolution: an `oxc_resolver` with a memoised specifier cache.
//!
//! Two resolvers, because a stylesheet is not resolved the way a module is. Sass has
//! its own rules — see [`Resolver::resolve`] — and running them through the
//! JavaScript resolver would find nothing.
//!
//! One of each per set of aliases in use. An alias belongs to the file that writes
//! the import rather than to the run, so two apps can mean different directories by
//! one name; the aliases are baked into a resolver when it is built, so resolvers are
//! cached by the chain of config directories that produced them. Most trees have one.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ahash::AHashMap;
use oxc_resolver::{
    PackageJson, Resolution, ResolveError, ResolveOptions, Resolver as OxcResolver,
    SideEffects as Declared, TsconfigDiscovery,
};

use crate::config::{Chain, Configs};

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

/// Specifiers a run asked for and could not place on disk.
///
/// Resolving to nothing is the quietest way this tool can be wrong. The edge is
/// dropped, the traversal carries on, and the verdict that comes out is a confident
/// "not affected" with no sign that anything went missing. Nothing is done about it
/// automatically — an import that names no file on this machine is usually a package
/// nobody installed rather than a fault in the tree — but `--unresolved` will say
/// what was asked for, so a whole app's worth of lost edges is something a person can
/// go and look at rather than something they have to already suspect.
///
/// Specifiers that name no file *by design* are not recorded: a Node builtin and a
/// `sass:` module are answers, not failures.
#[derive(Debug, Default)]
pub struct Unresolved {
    seen: RwLock<AHashMap<String, Vec<PathBuf>>>,
}

impl Unresolved {
    fn note(&self, from: &Path, specifier: &str) {
        let mut seen = self.seen.write().unwrap();
        let writers = seen.entry(specifier.to_string()).or_default();
        if !writers.iter().any(|known| known == from) {
            writers.push(from.to_path_buf());
        }
    }

    /// Each specifier with the files that wrote it, both in a settled order so that
    /// two runs over the same tree print the same report.
    pub fn sorted(&self) -> Vec<(String, Vec<PathBuf>)> {
        let mut out: Vec<(String, Vec<PathBuf>)> = self
            .seen
            .read()
            .unwrap()
            .iter()
            .map(|(specifier, writers)| {
                let mut writers = writers.clone();
                writers.sort();
                (specifier.clone(), writers)
            })
            .collect();
        out.sort();
        out
    }
}

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
    /// What each file's own directory chain declares. See [`crate::config`].
    configs: Arc<Configs>,
    /// Specifiers this run could not place. Shared, because a run builds more than
    /// one resolver and the report is about the run.
    unresolved: Arc<Unresolved>,
    /// One resolver per set of aliases, keyed by the config directories that produced
    /// them, which is the identity of the answer.
    modules: RwLock<AHashMap<Vec<PathBuf>, Arc<OxcResolver>>>,
    /// The same, tuned for Sass.
    style: RwLock<AHashMap<Vec<PathBuf>, Arc<OxcResolver>>>,
    cache: RwLock<AHashMap<(PathBuf, String), Option<PathBuf>>>,
    /// The `sideEffects` verdict for each path this resolver has produced, recorded
    /// while the resolution that found its `package.json` is still in hand.
    side_effects: RwLock<AHashMap<PathBuf, SideEffects>>,
    /// Where `node_modules` would be, for the nodes that stand for packages.
    root: PathBuf,
    /// Packages a lockfile change touched. See [`crate::lockfile`].
    packages: Arc<crate::lockfile::Changed>,
}

impl Resolver {
    /// `configs` is what the project declared about itself, which is the only place a
    /// stylesheet name that is not a path can come from. See [`crate::config`].
    pub fn new(
        configs: Arc<Configs>,
        unresolved: Arc<Unresolved>,
        root: PathBuf,
        packages: Arc<crate::lockfile::Changed>,
    ) -> Self {
        Self {
            configs,
            unresolved,
            root,
            packages,
            modules: RwLock::new(AHashMap::default()),
            style: RwLock::new(AHashMap::default()),
            cache: RwLock::new(AHashMap::default()),
            side_effects: RwLock::new(AHashMap::default()),
        }
    }

    /// The resolver for one chain of config directories, built on first use.
    fn module_resolver(&self, chain: &Chain) -> Arc<OxcResolver> {
        let key = chain.dirs().to_vec();
        if let Some(cached) = self.modules.read().unwrap().get(&key) {
            return cached.clone();
        }
        let resolver = Arc::new(OxcResolver::new(ResolveOptions {
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
            alias: chain.aliases().clone(),
            // So that `fs` and `node:fs` come back as themselves rather than as a
            // package nobody installed. They name no file, and saying so is what
            // keeps them out of the unresolved report.
            builtin_modules: true,
            // TypeScript makes a module specifier name the file the compiler will
            // emit, not the file on disk, so `./helper.js` is how a `.ts` file next
            // door is spelled — and `"#app/*": "./app/*.js"` is how a whole package
            // spells its own internals. Without this each of those resolves to
            // nothing, which is an edge lost in silence rather than an error.
            //
            // Each list has to end in the extension it came from. The lookup replaces
            // the normal one rather than adding to it, and refuses the file outright
            // when nothing in the list is there, so leaving `.js` out would stop a
            // real `.js` file from resolving at all.
            extension_alias: vec![
                (
                    ".js".to_string(),
                    vec![".ts".to_string(), ".tsx".to_string(), ".js".to_string()],
                ),
                (
                    ".jsx".to_string(),
                    vec![".tsx".to_string(), ".jsx".to_string()],
                ),
                (
                    ".cjs".to_string(),
                    vec![".cts".to_string(), ".cjs".to_string()],
                ),
                (
                    ".mjs".to_string(),
                    vec![".mts".to_string(), ".mjs".to_string()],
                ),
            ],
            ..ResolveOptions::default()
        }));
        self.modules.write().unwrap().insert(key, resolver.clone());
        resolver
    }

    /// The Sass resolver for one chain of config directories, built on first use.
    ///
    /// Sass looks for `_name.scss` beside `name.scss`, takes `_index.scss` for a
    /// directory, and tries the importing file's own directory before anything else —
    /// so a bare `@use "mixins"` is usually a sibling rather than a package. The
    /// `exports` field is left out because Sass tooling resolves a subpath by path,
    /// and honouring it would refuse targets that do resolve.
    fn style_resolver(&self, chain: &Chain) -> Arc<OxcResolver> {
        let key = chain.dirs().to_vec();
        if let Some(cached) = self.style.read().unwrap().get(&key) {
            return cached.clone();
        }
        let resolver = Arc::new(OxcResolver::new(ResolveOptions {
            extensions: vec![".scss".to_string(), ".css".to_string()],
            main_files: vec!["_index".to_string(), "index".to_string()],
            exports_fields: Vec::new(),
            prefer_relative: true,
            alias: chain.style_aliases().clone(),
            ..ResolveOptions::default()
        }));
        self.style.write().unwrap().insert(key, resolver.clone());
        resolver
    }

    /// Resolves `specifier` as written in `from_file`, or `None` if it does not
    /// point at a file on disk.
    pub fn resolve(&self, from_file: &Path, specifier: &str) -> Option<PathBuf> {
        let cache_key = (from_file.to_path_buf(), specifier.to_string());
        if let Some(cached) = self.cache.read().unwrap().get(&cache_key) {
            return cached.clone();
        }

        // A package whose lockfile entry changed stands for itself, rather than for
        // whichever of its files this import happens to name. The lockfile is the
        // only evidence that it changed and it speaks of packages, so the package is
        // what the graph has a node for — and it needs nothing installed to exist.
        if !is_style_file(from_file)
            && let Some(name) = crate::lockfile::package_of(specifier)
            && self.packages.contains(name)
        {
            let path = crate::lockfile::node_path(&self.root, name);
            self.cache
                .write()
                .unwrap()
                .insert(cache_key, Some(path.clone()));
            return Some(path);
        }

        // Whether the specifier names no file *by design*, which is a different thing
        // from one this run could not find and is not worth reporting as a failure.
        let mut names_no_file = false;
        let resolution = if is_style_file(from_file) {
            names_no_file = SASS_BUILTINS.contains(&specifier);
            self.resolve_style(from_file, specifier)
        } else {
            let resolver = self.module_resolver(&self.configs.chain(from_file));
            let mut attempt = resolver.resolve_file(from_file, specifier);
            if attempt.is_err()
                && let Some(request) = strip_inline_loaders(specifier)
            {
                attempt = resolver.resolve_file(from_file, request);
            }
            match attempt {
                Ok(found) => Some(found),
                Err(error) => {
                    names_no_file = matches!(error, ResolveError::Builtin { .. });
                    None
                }
            }
        };

        let result = resolution.map(|resolution| {
            let path = resolution.path().to_path_buf();
            self.side_effects
                .write()
                .unwrap()
                .insert(path.clone(), declared_side_effects(&resolution));
            path
        });

        if result.is_none() && !names_no_file {
            self.unresolved.note(from_file, specifier);
        }
        self.cache
            .write()
            .unwrap()
            .insert(cache_key, result.clone());
        result
    }

    /// Whether `path` is the node standing for a package whose lockfile entry
    /// changed. Asked by the searches, which have a resolver but no change set.
    pub fn marks_changed_package(&self, path: &Path) -> bool {
        self.packages.marks(&self.root, path)
    }

    /// Whether any package changed at all, which is the one thing a search
    /// cannot learn by asking about a path it has not reached yet.
    pub fn has_changed_packages(&self) -> bool {
        !self.packages.is_empty()
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

        let resolver = self.style_resolver(&self.configs.chain(from_file));
        std::iter::once(request.to_string())
            .chain(partial)
            .find_map(|candidate| resolver.resolve_file(from_file, &candidate).ok())
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
        Self::new(
            Arc::new(Configs::new(Path::new("."))),
            Arc::new(Unresolved::default()),
            PathBuf::from("."),
            Arc::new(crate::lockfile::Changed::default()),
        )
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
