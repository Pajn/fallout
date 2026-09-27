//! Module resolution: an `oxc_resolver` with a memoised specifier cache.
//!
//! The rules live in [`Tree`], which resolves over one file system: the disk as it
//! is, or the tree as it was before the change. [`Resolver`] resolves the way a run
//! does over the first, which adds the nodes for changed packages, the specifier
//! cache and the report of what could not be placed.
//!
//! More than one kind of resolver, because a stylesheet is not resolved the way a
//! module is. Sass has its own rules — see [`Tree::find`] and `resolve_sass` —
//! and running them through the JavaScript resolver would find nothing.
//!
//! One of each per set of aliases in use. An alias belongs to the file that writes
//! the import rather than to the run, so two apps can mean different directories by
//! one name; the aliases are baked into a resolver when it is built, so resolvers are
//! cached by the chain of config directories that produced them. Most trees have one.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ahash::AHashMap;
use oxc_resolver::{
    FileSystem, FileSystemOs, PackageJson, Resolution, ResolveError, ResolveOptions,
    ResolverGeneric, SideEffects as Declared, TsConfig, TsconfigDiscovery,
};

use crate::config::{Chain, Configs, Lookup};
use crate::module::is_style_file;
use crate::repoint::MovedImports;

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

    /// Adds everything `other` holds, for a report about several resolvers at once.
    pub fn absorb(&self, other: &Unresolved) {
        for (specifier, writers) in other.seen.read().unwrap().iter() {
            for writer in writers {
                self.note(writer, specifier);
            }
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

/// What kind of name a specifier that resolved to nothing was.
///
/// The first three name something in this repository — a file, or a name the
/// project declared for one — so nothing resolving is a gap in the graph a caller may
/// want to be told about. The last names a package nobody installed here, which is
/// usually not a fault in the tree at all.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnresolvedKind {
    /// A relative or absolute path to a file that is not there.
    Path,
    /// A name the project maps to its own files: a `fallout.toml` alias, a
    /// `package.json` `#import`, or a `tsconfig.json` `paths` entry or `baseUrl`.
    Alias,
    /// A package that is installed, or is one of the repository's own, with no entry
    /// the bundler's `[resolve]` settings select: an `exports` condition it does not
    /// match, a subpath it does not export, or a workspace package not yet linked.
    Package,
    /// A package that is not installed.
    MissingPackage,
}

impl UnresolvedKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UnresolvedKind::Path => "path",
            UnresolvedKind::Alias => "alias",
            UnresolvedKind::Package => "package",
            UnresolvedKind::MissingPackage => "missing-package",
        }
    }

    /// Whether the name is one this repository answers for.
    pub fn in_repo(self) -> bool {
        matches!(
            self,
            UnresolvedKind::Path | UnresolvedKind::Alias | UnresolvedKind::Package
        )
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

/// What looking for a specifier in one tree came to.
pub enum Found {
    /// The files it may load, each with the resolution that found it and the
    /// `package.json` it read. Never empty, and one file for anything but a Sass
    /// URL that more than one file answers: see `resolve_sass`.
    Files(Vec<Resolution>),
    /// A name that names no file by design, such as a Node builtin or a `sass:`
    /// module: an answer, not a failure.
    NoFile,
    /// Nothing, where something was asked for.
    NotFound,
}

impl Found {
    /// The files found, none if none was.
    pub fn paths(&self) -> Vec<&Path> {
        match self {
            Found::Files(resolutions) => resolutions.iter().map(Resolution::path).collect(),
            Found::NoFile | Found::NotFound => Vec::new(),
        }
    }
}

/// Which rules a resolver follows: a module's, a stylesheet's, or the exact file
/// lookup a Sass URL's candidates are looked for with.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum Dialect {
    Module,
    Style,
    Sass,
}

/// Resolution over one file system.
///
/// This is the one place the rules are, so that the tree before a change is asked
/// exactly what the tree now is: the same options, the same Sass rules, the same
/// handling of inline loaders. It answers only what is on the file system it looks
/// at. Standing a changed package in for its files, remembering answers and noting
/// what could not be placed are the [`Resolver`]'s, and a comparison between two
/// trees wants none of them.
pub struct Tree<Fs> {
    /// What each file's own directory chain declares. See [`crate::config`].
    configs: Arc<Configs>,
    /// Which `exports` conditions and entry fields the app's bundler reads.
    lookup: Lookup,
    fs: Fs,
    /// One resolver per dialect and set of aliases, keyed by the config directories
    /// that produced the aliases, which is the identity of the answer.
    resolvers: RwLock<Resolvers<Fs>>,
}

/// Resolvers by dialect and by the config directories that produced their aliases.
type Resolvers<Fs> = AHashMap<(Dialect, Vec<PathBuf>), Arc<ResolverGeneric<Fs>>>;

impl<Fs: FileSystem + Clone + 'static> Tree<Fs> {
    pub fn new(configs: Arc<Configs>, lookup: Lookup, fs: Fs) -> Self {
        Self {
            configs,
            lookup,
            fs,
            resolvers: RwLock::new(AHashMap::default()),
        }
    }

    /// The file system this tree looks at.
    pub fn fs(&self) -> &Fs {
        &self.fs
    }

    /// The same rules over another file system, which is how the tree before a
    /// change is built from the tree as it is.
    pub fn over<Other: FileSystem + Clone + 'static>(&self, fs: Other) -> Tree<Other> {
        Tree::new(self.configs.clone(), self.lookup.clone(), fs)
    }

    /// How `specifier` resolves from `from_file` in this tree.
    pub fn find(&self, from_file: &Path, specifier: &str) -> Found {
        let chain = self.configs.chain(from_file);
        if is_style_file(from_file) {
            if SASS_BUILTINS.contains(&specifier) {
                return Found::NoFile;
            }
            let request = specifier.strip_prefix('~').unwrap_or(specifier);
            let mut found = Vec::new();
            if is_sass_file(from_file) {
                found = resolve_sass(&self.resolver(Dialect::Sass, &chain), from_file, request);
            }
            // Where no candidate a Sass URL names is there, which is how a package's
            // entry point is named, and for plain CSS, the name is looked for as the
            // bundler looks for it.
            if found.is_empty() {
                let resolver = self.resolver(Dialect::Style, &chain);
                found.extend(resolve_style(&resolver, from_file, request));
            }
            return if found.is_empty() {
                Found::NotFound
            } else {
                Found::Files(found)
            };
        }
        let resolver = self.resolver(Dialect::Module, &chain);
        let mut attempt = resolver.resolve_file(from_file, specifier);
        if attempt.is_err()
            && let Some(request) = strip_inline_loaders(specifier)
        {
            attempt = resolver.resolve_file(from_file, request);
        }
        match attempt {
            Ok(resolution) => Found::Files(vec![resolution]),
            Err(ResolveError::Builtin { .. }) => Found::NoFile,
            Err(_) => Found::NotFound,
        }
    }

    /// The tsconfig that governs `file` in this tree, found the way resolving an
    /// import written in it finds it. An error for a tsconfig that cannot be parsed,
    /// or that names a config through `extends` that is not there.
    pub fn tsconfig_for(&self, file: &Path) -> Result<Option<Arc<TsConfig>>, ResolveError> {
        self.resolver(Dialect::Module, &self.configs.chain(file))
            .find_tsconfig(file)
    }

    /// The resolver for one dialect and chain of config directories, built on first
    /// use.
    fn resolver(&self, dialect: Dialect, chain: &Chain) -> Arc<ResolverGeneric<Fs>> {
        let key = (dialect, chain.dirs().to_vec());
        if let Some(cached) = self.resolvers.read().unwrap().get(&key) {
            return cached.clone();
        }
        let resolver = Arc::new(ResolverGeneric::new_with_file_system(
            self.fs.clone(),
            self.options(dialect, chain),
        ));
        self.resolvers
            .write()
            .unwrap()
            .insert(key, resolver.clone());
        resolver
    }

    fn options(&self, dialect: Dialect, chain: &Chain) -> ResolveOptions {
        match dialect {
            Dialect::Module => ResolveOptions {
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
                condition_names: self.lookup.conditions.clone(),
                main_fields: self.lookup.main_fields.clone(),
                // So that `fs` and `node:fs` come back as themselves rather than as a
                // package nobody installed. They name no file, and saying so is what
                // keeps them out of the unresolved report.
                builtin_modules: true,
                // TypeScript makes a module specifier name the file the compiler will
                // emit, not the file on disk, so `./helper.js` is how a `.ts` file
                // next door is spelled — and `"#app/*": "./app/*.js"` is how a whole
                // package spells its own internals. Without this each of those
                // resolves to nothing, which is an edge lost in silence rather than an
                // error.
                //
                // Each list has to end in the extension it came from. The lookup
                // replaces the normal one rather than adding to it, and refuses the
                // file outright when nothing in the list is there, so leaving `.js`
                // out would stop a real `.js` file from resolving at all.
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
            },
            // A stylesheet tries the importing file's own directory before anything
            // else — so a bare `@use "mixins"` is usually a sibling rather than a
            // package. The `exports` field is left out because Sass tooling resolves
            // a subpath by path, and honouring it would refuse targets that do
            // resolve. This lookup takes `_index.scss` for a directory, and is the
            // one plain CSS gets, and a Sass URL none of whose candidates is there.
            Dialect::Style => ResolveOptions {
                extensions: vec![".scss".to_string(), ".css".to_string()],
                main_files: vec!["_index".to_string(), "index".to_string()],
                exports_fields: Vec::new(),
                prefer_relative: true,
                alias: chain.style_aliases().clone(),
                ..ResolveOptions::default()
            },
            // The same, finding only the file a candidate names: which extensions
            // and partials are tried, and in what order, is the Sass spec's to say,
            // and `resolve_sass` says it one file name at a time.
            Dialect::Sass => ResolveOptions {
                extensions: Vec::new(),
                main_files: Vec::new(),
                exports_fields: Vec::new(),
                prefer_relative: true,
                alias: chain.style_aliases().clone(),
                ..ResolveOptions::default()
            },
        }
    }
}

/// The files each `(importing file, specifier)` pair resolved to.
type Answers = AHashMap<(PathBuf, String), Arc<[PathBuf]>>;

/// Resolves import specifiers to absolute paths, caching every answer
/// (including failures) per `(importing file, specifier)` pair.
pub struct Resolver {
    /// What each file's own directory chain declares. See [`crate::config`].
    configs: Arc<Configs>,
    /// Specifiers this run could not place. Shared, because a run builds more than
    /// one resolver and the report is about the run.
    unresolved: Arc<Unresolved>,
    /// The tree as it is.
    now: Arc<Tree<FileSystemOs>>,
    /// Which imports the change may have moved, as this resolver resolves them. See
    /// [`crate::repoint`].
    moved: MovedImports,
    cache: RwLock<Answers>,
    /// The `sideEffects` verdict for each path this resolver has produced, recorded
    /// while the resolution that found its `package.json` is still in hand.
    side_effects: RwLock<AHashMap<PathBuf, SideEffects>>,
    /// Where `node_modules` would be, for the nodes that stand for packages.
    root: PathBuf,
    /// Packages a lockfile change touched. See [`crate::lockfile`].
    packages: Arc<crate::lockfile::Changed>,
    /// The names of the repository's own packages, read when first asked.
    workspace: std::sync::OnceLock<ahash::AHashSet<String>>,
}

impl Resolver {
    /// `configs` is what the project declared about itself, which is the only place a
    /// stylesheet name that is not a path can come from. See [`crate::config`].
    /// `lookup` is which `exports` conditions and entry fields the app's bundler
    /// reads: anchors whose bundlers differ get resolvers of their own.
    pub fn new(
        configs: Arc<Configs>,
        unresolved: Arc<Unresolved>,
        root: PathBuf,
        packages: Arc<crate::lockfile::Changed>,
        repointing: Arc<crate::repoint::Repointing>,
        lookup: Lookup,
    ) -> Self {
        let now = Arc::new(Tree::new(
            configs.clone(),
            lookup.clone(),
            FileSystemOs::new(),
        ));
        let moved = MovedImports::new(repointing, now.clone(), root.clone());
        Self {
            configs,
            unresolved,
            now,
            moved,
            root,
            packages,
            workspace: std::sync::OnceLock::new(),
            cache: RwLock::new(AHashMap::default()),
            side_effects: RwLock::new(AHashMap::default()),
        }
    }

    /// Which of the imports of `file` the change may have sent to another file, by
    /// index into `specifiers`, which are read only the first time a file is asked
    /// about. See [`crate::repoint`].
    ///
    /// The answer is kept here, and not on the change, because it is this
    /// resolver's: another bundler may resolve the same file otherwise.
    pub fn moved<S: AsRef<[String]>>(
        &self,
        file: &Path,
        specifiers: impl FnOnce() -> S,
    ) -> Arc<[usize]> {
        self.moved.moved(file, specifiers)
    }

    /// The specifiers this resolver could not place.
    pub fn unresolved(&self) -> &Unresolved {
        &self.unresolved
    }

    /// What kind of name `specifier`, written in `from_file`, is. Asked only of one
    /// that resolved to nothing.
    pub fn unresolved_kind(&self, from_file: &Path, specifier: &str) -> UnresolvedKind {
        let specifier = strip_inline_loaders(specifier).unwrap_or(specifier);
        if specifier.starts_with('.') || specifier.starts_with('/') {
            return UnresolvedKind::Path;
        }
        if specifier.starts_with('#') {
            return UnresolvedKind::Alias;
        }
        // A stylesheet names a sibling by a bare name, and a package with `~`.
        let specifier = if is_style_file(from_file) {
            match specifier.strip_prefix('~') {
                Some(package) => package,
                None if !self.style_aliased(from_file, specifier) => {
                    return UnresolvedKind::Path;
                }
                None => specifier,
            }
        } else {
            specifier
        };
        let chain = self.configs.chain(from_file);
        let aliased = chain
            .aliases()
            .iter()
            .chain(chain.style_aliases().iter())
            .any(|(name, _)| alias_matches(name, specifier));
        if aliased {
            return UnresolvedKind::Alias;
        }
        if package_installed(from_file, specifier) || self.workspace_package(specifier) {
            return UnresolvedKind::Package;
        }
        let mapped = self
            .now
            .tsconfig_for(from_file)
            .ok()
            .flatten()
            .is_some_and(|tsconfig| {
                !tsconfig
                    .resolve_path_alias_or_base_url(specifier)
                    .is_empty()
            });
        if mapped {
            UnresolvedKind::Alias
        } else {
            UnresolvedKind::MissingPackage
        }
    }

    fn style_aliased(&self, from_file: &Path, specifier: &str) -> bool {
        self.configs
            .chain(from_file)
            .style_aliases()
            .iter()
            .any(|(name, _)| alias_matches(name, specifier))
    }

    /// Whether a bare specifier names one of the repository's own packages, which a
    /// workspace links rather than installs. Every `package.json` under the root
    /// that is not in a `node_modules` is read once, for its `name`.
    fn workspace_package(&self, specifier: &str) -> bool {
        let Some(name) = package_name(specifier) else {
            return false;
        };
        self.workspace
            .get_or_init(|| {
                walkdir::WalkDir::new(&self.root)
                    .into_iter()
                    .filter_entry(|entry| {
                        let name = entry.file_name().to_string_lossy();
                        name != "node_modules" && !(entry.depth() > 0 && name.starts_with('.'))
                    })
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_name() == "package.json")
                    .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                    .filter_map(|text| declared_name(&text))
                    .collect()
            })
            .contains(&name)
    }

    /// Resolves `specifier` as written in `from_file` to the files on disk it may
    /// load: none, one, or for a Sass URL several files answer, each of them.
    pub fn resolve(&self, from_file: &Path, specifier: &str) -> Arc<[PathBuf]> {
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
            let path: Arc<[PathBuf]> = Arc::from([crate::lockfile::node_path(&self.root, name)]);
            self.cache.write().unwrap().insert(cache_key, path.clone());
            return path;
        }

        let result: Arc<[PathBuf]> = match self.now.find(from_file, specifier) {
            Found::Files(resolutions) => {
                let mut side_effects = self.side_effects.write().unwrap();
                resolutions
                    .iter()
                    .map(|resolution| {
                        let path = resolution.path().to_path_buf();
                        side_effects.insert(path.clone(), declared_side_effects(resolution));
                        path
                    })
                    .collect()
            }
            // A specifier that names no file *by design* is a different thing from
            // one this run could not find, and is not worth reporting as a failure.
            Found::NoFile => Arc::from([]),
            Found::NotFound => {
                self.unresolved.note(from_file, specifier);
                Arc::from([])
            }
        };
        self.cache
            .write()
            .unwrap()
            .insert(cache_key, result.clone());
        result
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

/// Resolves `request`, written in a Sass stylesheet with any leading `~` dropped,
/// the way the Sass spec resolves a `file:` URL (spec/modules.md, "Resolving a
/// `file:` URL"): every file that may be the one it loads, or none.
///
/// A URL with no extension is tried as `.sass` and `.scss`, each as written and as
/// the partial Sass keeps under a leading underscore, then as `.css` the same way.
/// Only when none of those is there is it tried as `url/index`, in the same order.
/// A URL with an extension is tried as written and as a partial. Where more than
/// one file answers one step Sass refuses the URL, and which one the author meant
/// is not known, so every one of them is kept.
///
/// An `@import` tries the import-only `.import.sass`, `.import.scss` and
/// `.import.css` first. Which rule wrote a specifier is not kept, so both readings
/// are taken, and they differ only where there is an import-only file.
fn resolve_sass<Fs: FileSystem>(
    exact: &ResolverGeneric<Fs>,
    from_file: &Path,
    request: &str,
) -> Vec<Resolution> {
    let exists = |candidate: &str| exact.resolve_file(from_file, candidate).ok();
    let mut found: Vec<Resolution> = Vec::new();
    for import in [false, true] {
        let url = sass_for_extensions(&exists, request, import);
        let url = if url.is_empty() {
            sass_for_extensions(&exists, &format!("{request}/index"), import)
        } else {
            url
        };
        for resolution in url {
            if !found.iter().any(|known| known.path() == resolution.path()) {
                found.push(resolution);
            }
        }
    }
    found
}

/// "Resolving a `file:` URL for extensions", keeping every file of the first step
/// that has any.
fn sass_for_extensions(
    exists: &impl Fn(&str) -> Option<Resolution>,
    url: &str,
    import: bool,
) -> Vec<Resolution> {
    if let Some(suffix) = [".scss", ".sass", ".css"]
        .into_iter()
        .find(|suffix| url.ends_with(suffix))
    {
        if import {
            let prefix = &url[..url.len() - suffix.len()];
            let found = sass_for_partials(exists, &format!("{prefix}.import{suffix}"));
            if !found.is_empty() {
                return found;
            }
        }
        return sass_for_partials(exists, url);
    }
    let steps: &[&[&str]] = if import {
        &[
            &[".import.sass", ".import.scss"],
            &[".import.css"],
            &[".sass", ".scss"],
            &[".css"],
        ]
    } else {
        &[&[".sass", ".scss"], &[".css"]]
    };
    for step in steps {
        let found: Vec<Resolution> = step
            .iter()
            .flat_map(|extension| sass_for_partials(exists, &format!("{url}{extension}")))
            .collect();
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

/// "Resolving a `file:` URL for partials": the file as named and the partial beside
/// it, both where both are there.
fn sass_for_partials(exists: &impl Fn(&str) -> Option<Resolution>, url: &str) -> Vec<Resolution> {
    let (directory, base) = match url.rsplit_once('/') {
        Some((directory, base)) => (Some(directory), base),
        None => (None, url),
    };
    if base.starts_with('_') {
        return exists(url).into_iter().collect();
    }
    let partial = match directory {
        Some(directory) => format!("{directory}/_{base}"),
        None => format!("_{base}"),
    };
    [url, partial.as_str()]
        .into_iter()
        .filter_map(exists)
        .collect()
}

/// Whether `path` is written in Sass, which resolves its URLs by its own rules,
/// rather than in plain CSS.
fn is_sass_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "scss" || extension == "sass")
}

/// Resolves `request`, with any leading `~` dropped, the way a bundler looks for a
/// stylesheet: as written, then as a partial.
///
/// A leading `~` is a bundler convention meaning "not relative", and what follows
/// it is an ordinary request. A partial is tried because Sass keeps a file meant
/// only for importing under a leading underscore and lets it be named without one,
/// so `a/b` is also `a/_b`.
fn resolve_style<Fs: FileSystem>(
    resolver: &ResolverGeneric<Fs>,
    from_file: &Path,
    request: &str,
) -> Option<Resolution> {
    let partial = match request.rsplit_once('/') {
        Some((dir, base)) if !base.starts_with('_') => Some(format!("{dir}/_{base}")),
        None if !request.starts_with('_') => Some(format!("_{request}")),
        _ => None,
    };

    std::iter::once(request.to_string())
        .chain(partial)
        .find_map(|candidate| resolver.resolve_file(from_file, &candidate).ok())
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

/// Whether an alias written as `name` covers `specifier`, read the way the resolver
/// reads it: `name$` only exactly, `name*` by what comes before the star, and a
/// plain name exactly or as a directory.
fn alias_matches(name: &str, specifier: &str) -> bool {
    if let Some(exact) = name.strip_suffix('$') {
        return specifier == exact;
    }
    if let Some((prefix, _)) = name.split_once('*') {
        return specifier.starts_with(prefix);
    }
    specifier == name
        || specifier
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether `path` belongs to an installed package, somewhere below a `node_modules`
/// directory, rather than to the project's own source.
pub fn is_installed(path: &Path) -> bool {
    path.components()
        .any(|part| part.as_os_str() == "node_modules")
}

/// Whether the package a bare specifier names is installed where Node would look
/// for it from `from_file`: a `node_modules` in its directory or any above it.
fn package_installed(from_file: &Path, specifier: &str) -> bool {
    let Some(name) = package_name(specifier) else {
        return false;
    };
    from_file
        .ancestors()
        .skip(1)
        .any(|dir| dir.join("node_modules").join(&name).is_dir())
}

/// The package a bare specifier names: `@scope/name` or `name`.
fn package_name(specifier: &str) -> Option<String> {
    let mut segments = specifier.split('/');
    match segments.next()? {
        scope if scope.starts_with('@') => Some(format!("{scope}/{}", segments.next()?)),
        "" => None,
        package => Some(package.to_string()),
    }
}

/// The top-level `name` a `package.json` declares, read without a JSON parser: the
/// first `"name"` key at the first level of nesting.
fn declared_name(text: &str) -> Option<String> {
    let mut depth = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((at, character)) = chars.next() {
        match character {
            '{' | '[' => depth += 1,
            '}' | ']' => depth = depth.saturating_sub(1),
            '"' => {
                let start = at + 1;
                let mut end = start;
                let mut escaped = false;
                for (index, inner) in chars.by_ref() {
                    if escaped {
                        escaped = false;
                    } else if inner == '\\' {
                        escaped = true;
                    } else if inner == '"' {
                        end = index;
                        break;
                    }
                }
                if depth != 1 || &text[start..end] != "name" {
                    continue;
                }
                // A `"name"` that is a value, or a key whose value is not a string,
                // is not the package's name, which may still come later.
                let value = text[end + 1..]
                    .trim_start()
                    .strip_prefix(':')
                    .and_then(|rest| rest.trim_start().strip_prefix('"'));
                if let Some(name) = value.and_then(|value| Some(&value[..value.find('"')?])) {
                    return Some(name.to_string());
                }
            }
            _ => {}
        }
    }
    None
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
    fn a_package_name_is_its_top_level_name_key() {
        let name = |text: &str| declared_name(text);
        assert_eq!(
            name(r#"{ "name": "@acme/ui" }"#).as_deref(),
            Some("@acme/ui")
        );
        // A `"name"` that is a value, or nested, or a key with no string, is not it.
        assert_eq!(
            name(r#"{ "description": "name", "name": "@acme/ui" }"#).as_deref(),
            Some("@acme/ui")
        );
        assert_eq!(
            name(r#"{ "exports": { "name": "x" }, "name": "@acme/ui" }"#).as_deref(),
            Some("@acme/ui")
        );
        assert_eq!(name(r#"{ "name": null }"#), None);
    }

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
