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
    FileSystem, PackageJson, Resolution, ResolveError, ResolveOptions, Resolver as OxcResolver,
    ResolverGeneric, SideEffects as Declared, TsconfigDiscovery,
};

use crate::config::{Chain, Configs, Lookup};

/// A resolver over the tree as it was before the change.
type BeforeResolver = ResolverGeneric<crate::repoint::BeforeFs>;

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
    /// Both again, over the tree as it was before the change. See [`crate::repoint`].
    before_modules: RwLock<AHashMap<Vec<PathBuf>, Arc<BeforeResolver>>>,
    before_style: RwLock<AHashMap<Vec<PathBuf>, Arc<BeforeResolver>>>,
    cache: RwLock<AHashMap<(PathBuf, String), Option<PathBuf>>>,
    /// The `sideEffects` verdict for each path this resolver has produced, recorded
    /// while the resolution that found its `package.json` is still in hand.
    side_effects: RwLock<AHashMap<PathBuf, SideEffects>>,
    /// Where `node_modules` would be, for the nodes that stand for packages.
    root: PathBuf,
    /// Packages a lockfile change touched. See [`crate::lockfile`].
    packages: Arc<crate::lockfile::Changed>,
    /// What the change could have sent an import to instead. See [`crate::repoint`].
    repointing: Arc<crate::repoint::Repointing>,
    /// The configuration files each tsconfig reads, itself first.
    tsconfig_reads: RwLock<AHashMap<PathBuf, Arc<[PathBuf]>>>,
    /// Which `exports` conditions and entry fields the app's bundler reads. One per
    /// resolver: anchors whose bundlers differ get resolvers of their own.
    lookup: Lookup,
    /// The names of the repository's own packages, read when first asked.
    workspace: std::sync::OnceLock<ahash::AHashSet<String>>,
    /// Finds the config a package `extends` entry names, built on first use.
    extends: std::sync::OnceLock<OxcResolver>,
    /// What each file imports, resolved, for the searches that walk file by file.
    /// Several anchors' searches walk the same files, and reading one means parsing
    /// it.
    imports: RwLock<AHashMap<PathBuf, Arc<[PathBuf]>>>,
    /// Whether each file imports something the change may have moved, for the same
    /// searches, which would otherwise read the file again to ask. Kept here because
    /// the answer is this resolver's: another bundler may resolve the file otherwise.
    repoints: RwLock<AHashMap<PathBuf, bool>>,
}

impl Resolver {
    /// `configs` is what the project declared about itself, which is the only place a
    /// stylesheet name that is not a path can come from. See [`crate::config`].
    pub fn new(
        configs: Arc<Configs>,
        unresolved: Arc<Unresolved>,
        root: PathBuf,
        packages: Arc<crate::lockfile::Changed>,
        repointing: Arc<crate::repoint::Repointing>,
        lookup: Lookup,
    ) -> Self {
        Self {
            configs,
            unresolved,
            root,
            packages,
            repointing,
            tsconfig_reads: RwLock::new(AHashMap::default()),
            lookup,
            workspace: std::sync::OnceLock::new(),
            extends: std::sync::OnceLock::new(),
            imports: RwLock::new(AHashMap::default()),
            repoints: RwLock::new(AHashMap::default()),
            modules: RwLock::new(AHashMap::default()),
            style: RwLock::new(AHashMap::default()),
            before_modules: RwLock::new(AHashMap::default()),
            before_style: RwLock::new(AHashMap::default()),
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
        let resolver = Arc::new(OxcResolver::new(self.module_options(chain)));
        self.modules.write().unwrap().insert(key, resolver.clone());
        resolver
    }

    /// The same over the tree before the change.
    fn before_module_resolver(&self, chain: &Chain) -> Arc<BeforeResolver> {
        let key = chain.dirs().to_vec();
        if let Some(cached) = self.before_modules.read().unwrap().get(&key) {
            return cached.clone();
        }
        let resolver = Arc::new(BeforeResolver::new_with_file_system(
            self.repointing.file_system(),
            self.module_options(chain),
        ));
        self.before_modules
            .write()
            .unwrap()
            .insert(key, resolver.clone());
        resolver
    }

    fn module_options(&self, chain: &Chain) -> ResolveOptions {
        ResolveOptions {
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
        }
    }

    /// The Sass resolver for one chain of config directories, built on first use.
    /// See [`style_options`].
    fn style_resolver(&self, chain: &Chain) -> Arc<OxcResolver> {
        let key = chain.dirs().to_vec();
        if let Some(cached) = self.style.read().unwrap().get(&key) {
            return cached.clone();
        }
        let resolver = Arc::new(OxcResolver::new(style_options(chain)));
        self.style.write().unwrap().insert(key, resolver.clone());
        resolver
    }

    /// The same over the tree before the change.
    fn before_style_resolver(&self, chain: &Chain) -> Arc<BeforeResolver> {
        let key = chain.dirs().to_vec();
        if let Some(cached) = self.before_style.read().unwrap().get(&key) {
            return cached.clone();
        }
        let resolver = Arc::new(BeforeResolver::new_with_file_system(
            self.repointing.file_system(),
            style_options(chain),
        ));
        self.before_style
            .write()
            .unwrap()
            .insert(key, resolver.clone());
        resolver
    }

    /// Whether `file` imports something the change may have moved, worked out by
    /// `ask` the first time it is asked. See [`crate::change::Change::repoints`].
    pub fn repoints_of(&self, file: &Path, ask: impl FnOnce() -> bool) -> bool {
        if let Some(&known) = self.repoints.read().unwrap().get(file) {
            return known;
        }
        let repoints = ask();
        self.repoints
            .write()
            .unwrap()
            .insert(file.to_path_buf(), repoints);
        repoints
    }

    /// The files `file` imports, worked out by `read` the first time it is asked.
    pub fn imports_of(&self, file: &Path, read: impl FnOnce() -> Vec<PathBuf>) -> Arc<[PathBuf]> {
        if let Some(known) = self.imports.read().unwrap().get(file) {
            return known.clone();
        }
        let imports: Arc<[PathBuf]> = read().into();
        self.imports
            .write()
            .unwrap()
            .insert(file.to_path_buf(), imports.clone());
        imports
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
            .module_resolver(&chain)
            .find_tsconfig(from_file)
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
        let chain = self.configs.chain(from_file);
        let found = find(
            || self.module_resolver(&chain),
            || self.style_resolver(&chain),
            from_file,
            specifier,
        );
        let (resolution, names_no_file) = match found {
            Ok(resolution) => (Some(resolution), false),
            Err(names_no_file) => (None, names_no_file),
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

    /// Which of `specifiers`, imported from `from_file`, the change may have sent
    /// to another file, by index. See [`crate::repoint`].
    ///
    /// An import has moved when the tree before the change resolves it to another
    /// file, or to none, or to one where there is none now.
    pub fn moved(&self, from_file: &Path, specifiers: &[String]) -> Vec<usize> {
        if self.repointing.is_empty() {
            return Vec::new();
        }
        if self.governed_by_unknown_config(from_file) {
            return (0..specifiers.len()).collect();
        }
        if !self.repointing.has_before() {
            return Vec::new();
        }
        let chain = self.configs.chain(from_file);
        let path =
            |found: Result<Resolution, bool>| found.ok().map(|found| found.path().to_path_buf());
        specifiers
            .iter()
            .enumerate()
            .filter(|(_, specifier)| {
                let now = find(
                    || self.module_resolver(&chain),
                    || self.style_resolver(&chain),
                    from_file,
                    specifier,
                );
                let then = find(
                    || self.before_module_resolver(&chain),
                    || self.before_style_resolver(&chain),
                    from_file,
                    specifier,
                );
                path(now) != path(then)
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Whether a changed config whose earlier version is not known may govern
    /// `file`, which then has to be taken as moving all its imports.
    ///
    /// A config governs a file when the tsconfig found for it reads the config,
    /// directly or through `extends`, and so does one of the `tsconfig.json` files
    /// above it, which can decide through `references`, `include` or `exclude` which
    /// config is found. A changed `tsconfig.json` above the file governs it too,
    /// whatever it reads now, since it may have been the nearest before. A tsconfig
    /// that cannot be read at all is taken to govern everything: what it said before
    /// is exactly what is not known.
    fn governed_by_unknown_config(&self, file: &Path) -> bool {
        let configs = self.repointing.configs();
        if configs.is_empty() {
            return false;
        }
        let mut reads: Vec<PathBuf> = Vec::new();
        match self
            .module_resolver(&self.configs.chain(file))
            .find_tsconfig(file)
        {
            Ok(Some(tsconfig)) => {
                reads.extend(self.tsconfig_reads(tsconfig.path()).iter().cloned())
            }
            Ok(None) => {}
            Err(_) => return true,
        }
        for directory in file
            .ancestors()
            .skip(1)
            .take_while(|directory| directory.starts_with(&self.root))
        {
            let above = directory.join("tsconfig.json");
            if above.is_file() {
                reads.extend(self.tsconfig_reads(&above).iter().cloned());
            }
        }
        configs.iter().any(|config| {
            reads.contains(config)
                || (config
                    .file_name()
                    .is_some_and(|name| name == "tsconfig.json")
                    && config
                        .parent()
                        .is_some_and(|directory| file.starts_with(directory)))
        })
    }

    /// Every configuration file the tsconfig at `path` reads: itself, what it
    /// extends, and what it references, however far.
    fn tsconfig_reads(&self, path: &Path) -> Arc<[PathBuf]> {
        if let Some(known) = self.tsconfig_reads.read().unwrap().get(path) {
            return known.clone();
        }
        let mut reads = vec![path.to_path_buf()];
        let mut next = 0;
        while let Some(config) = reads.get(next).cloned() {
            next += 1;
            let Some(parsed) = std::fs::read_to_string(&config)
                .ok()
                .and_then(|text| oxc_resolver::TsConfig::parse(true, &config, &config, text).ok())
            else {
                continue;
            };
            let extends = match &parsed.extends {
                Some(oxc_resolver::ExtendsField::Single(one)) => vec![one.clone()],
                Some(oxc_resolver::ExtendsField::Multiple(many)) => many.clone(),
                None => Vec::new(),
            };
            let referenced = parsed.references.iter().filter_map(|reference| {
                let path = crate::repoint::normalize(&config.parent()?.join(&reference.path));
                Some(
                    if path
                        .extension()
                        .is_some_and(|extension| extension == "json")
                    {
                        path
                    } else {
                        path.join("tsconfig.json")
                    },
                )
            });
            let named: Vec<PathBuf> = extends
                .iter()
                .flat_map(|specifier| self.extended_configs(&config, specifier))
                .chain(referenced.map(|path| dunce::canonicalize(&path).unwrap_or(path)))
                .collect();
            for read in named {
                if !reads.contains(&read) {
                    reads.push(read);
                }
            }
        }
        let reads: Arc<[PathBuf]> = reads.into();
        self.tsconfig_reads
            .write()
            .unwrap()
            .insert(path.to_path_buf(), reads.clone());
        reads
    }

    /// The files an `extends` entry of the tsconfig at `config` may name, whether or
    /// not they are still there.
    ///
    /// A relative entry written without `.json` may name a file with it added, or a
    /// directory's `tsconfig.json`, and which one is a question about the disk before
    /// the change as much as after it, so both are read.
    fn extended_configs(&self, config: &Path, specifier: &str) -> Vec<PathBuf> {
        let Some(directory) = config.parent() else {
            return Vec::new();
        };
        if specifier.starts_with('.') || Path::new(specifier).is_absolute() {
            let path = crate::repoint::normalize(&directory.join(specifier));
            let mut named = vec![path.join("tsconfig.json")];
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                named.push(path);
            } else {
                let mut file = path;
                file.as_mut_os_string().push(".json");
                named.push(file);
            }
            return named
                .into_iter()
                .map(|path| dunce::canonicalize(&path).unwrap_or(path))
                .collect();
        }
        // A package's config, looked for the way oxc_resolver looks for it when it
        // follows `extends`: a JSON file, and a package's `tsconfig.json` where
        // the entry names only the package. A module resolver would find the
        // package's code instead.
        let resolver = self.extends.get_or_init(|| {
            OxcResolver::new(ResolveOptions {
                condition_names: vec!["node".to_string(), "import".to_string()],
                extensions: vec![".json".to_string()],
                main_files: vec!["tsconfig".to_string()],
                ..ResolveOptions::default()
            })
        });
        resolver
            .resolve(directory, specifier)
            .ok()
            .map(|resolution| resolution.full_path())
            .into_iter()
            .collect()
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

/// How `specifier` resolves from `from_file`, with the resolvers `module` and
/// `style` give: the one place the rules are, so that the tree before a change is
/// asked exactly what the tree now is. `Err(true)` for a specifier that names no
/// file by design, such as a Node builtin.
fn find<Fs: FileSystem>(
    module: impl FnOnce() -> Arc<ResolverGeneric<Fs>>,
    style: impl FnOnce() -> Arc<ResolverGeneric<Fs>>,
    from_file: &Path,
    specifier: &str,
) -> Result<Resolution, bool> {
    if is_style_file(from_file) {
        return resolve_style(&style(), from_file, specifier)
            .ok_or(SASS_BUILTINS.contains(&specifier));
    }
    let resolver = module();
    let mut attempt = resolver.resolve_file(from_file, specifier);
    if attempt.is_err()
        && let Some(request) = strip_inline_loaders(specifier)
    {
        attempt = resolver.resolve_file(from_file, request);
    }
    attempt.map_err(|error| matches!(error, ResolveError::Builtin { .. }))
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
fn resolve_style<Fs: FileSystem>(
    resolver: &ResolverGeneric<Fs>,
    from_file: &Path,
    specifier: &str,
) -> Option<Resolution> {
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
        .find_map(|candidate| resolver.resolve_file(from_file, &candidate).ok())
}

/// How a stylesheet's imports are looked for.
///
/// Sass looks for `_name.scss` beside `name.scss`, takes `_index.scss` for a
/// directory, and tries the importing file's own directory before anything else —
/// so a bare `@use "mixins"` is usually a sibling rather than a package. The
/// `exports` field is left out because Sass tooling resolves a subpath by path,
/// and honouring it would refuse targets that do resolve.
fn style_options(chain: &Chain) -> ResolveOptions {
    ResolveOptions {
        extensions: vec![".scss".to_string(), ".css".to_string()],
        main_files: vec!["_index".to_string(), "index".to_string()],
        exports_fields: Vec::new(),
        prefer_relative: true,
        alias: chain.style_aliases().clone(),
        ..ResolveOptions::default()
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
            Arc::default(),
            Lookup::default(),
        )
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
