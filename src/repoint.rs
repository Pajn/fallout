//! Imports a change may have pointed somewhere else.
//!
//! An import names a file through the rules that resolve it, and a change can move
//! the file without touching the import. A `tsconfig.json` whose `paths` stop
//! mapping `@reduxjs/toolkit` to an app's own shim, or the shim deleted from under
//! the mapping, sends the same specifier to the package, and the file that writes it
//! is as changed as if its import had been rewritten. Nothing in the graph of the
//! current version says so: it only knows where each import goes now.
//!
//! So a run keeps the tree as it was before the change, as far as the change says
//! how it differed, and a search resolves each import it meets there as well as in
//! the tree as it is. An import that the two answer differently has moved. Asking
//! the resolver, rather than modelling what TypeScript would do, is what keeps
//! `paths` fallbacks, `rootDirs`, `extends` chains, `references`, package `main`
//! fields, Sass partials and every rule nobody listed answering the same way the
//! graph's own edges do.
//!
//! The tree as it was is the current one with these differences:
//!
//! - a deleted file is there again, and so is a file a rename took away;
//! - a file the change added is not;
//! - with a base revision, a changed JSON file holds what it held then, which is what
//!   a tsconfig or a `package.json` changing its mind looks like.
//!
//! Without a base revision there is no earlier text for a changed config. It is then
//! taken to move every import of the files it governs.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use ahash::{AHashMap, AHashSet};
use oxc_resolver::{
    FileMetadata, FileSystem, FileSystemOs, ResolveError, ResolveOptions, ResolverGeneric,
};

use crate::base::Earlier;
use crate::diff::{ChangeSet, FileChange};
use crate::resolve::Tree;

/// What a change could have moved, as a run asks about it.
#[derive(Debug, Default)]
pub struct Repointing {
    before: Arc<Before>,
    /// Changed configs with no earlier text to compare with, which a run without a
    /// base revision has.
    configs: Vec<PathBuf>,
}

/// How the tree before the change differs from the one on disk.
#[derive(Debug, Default)]
pub struct Before {
    /// What each path held, or `None` for a path that was not there.
    files: AHashMap<PathBuf, Option<Arc<[u8]>>>,
    /// Directories that were there only because a file in `files` was.
    dirs: AHashSet<PathBuf>,
}

impl Repointing {
    pub fn is_empty(&self) -> bool {
        self.before.files.is_empty() && self.configs.is_empty()
    }

    /// Whether any path is different in the tree before the change, which is when
    /// resolving there can give another answer.
    pub fn has_before(&self) -> bool {
        !self.before.files.is_empty()
    }

    /// `base`, the tree as it is, as it was before the change.
    pub fn over<Fs: FileSystem>(&self, base: Fs) -> BeforeFs<Fs> {
        BeforeFs {
            before: self.before.clone(),
            base,
        }
    }

    /// Changed configs whose earlier version is not known.
    pub fn configs(&self) -> &[PathBuf] {
        &self.configs
    }
}

impl Before {
    /// Records what `path` held, where `is_dir` says which directories the tree as
    /// it is still has.
    fn put(&mut self, path: PathBuf, content: Option<Arc<[u8]>>, is_dir: impl Fn(&Path) -> bool) {
        if content.is_some() {
            for directory in path.ancestors().skip(1) {
                if is_dir(directory) || !self.dirs.insert(directory.to_path_buf()) {
                    break;
                }
            }
        }
        self.files.insert(path, content);
    }
}

/// What a change set, and the paths named as changed beside it, could have moved,
/// read once per run.
///
/// A named path that is not there is taken as deleted. Every changed JSON file is
/// kept as it was, since whether a resolver reads it is a question for each import:
/// `extends` can name any file.
pub fn repointing(
    root: &Path,
    changes: &ChangeSet,
    explicit: &[PathBuf],
    earlier: Option<&dyn Earlier>,
) -> Repointing {
    let added: AHashSet<PathBuf> = changes
        .added
        .iter()
        .map(|path| canonical(&root.join(path)))
        .collect();
    let named = changes
        .files
        .iter()
        .map(|file| (root.join(&file.path), file.change == FileChange::Deleted))
        .chain(
            changes
                .renamed_from
                .iter()
                .map(|path| (root.join(path), true)),
        )
        .chain(explicit.iter().map(|path| (path.clone(), !path.exists())));

    let mut repointing = Repointing::default();
    let mut before = Before::default();
    let earlier_text = |path: &Path| earlier.and_then(|earlier| earlier.text(path));
    for (path, deleted) in named {
        let path = canonical(&path);
        if before.files.contains_key(&path) || repointing.configs.contains(&path) {
            continue;
        }
        let json = path
            .extension()
            .is_some_and(|extension| extension == "json");
        if deleted {
            // A file's contents matter to a resolver only when it reads the file,
            // which it does for JSON: a config, or a package's manifest.
            // A deleted file was there, whatever it held, so it is put back either
            // way. A JSON file whose earlier text cannot be read, with no base
            // revision or none git can show, is a config whose earlier version is
            // not known.
            let content = if json { earlier_text(&path) } else { None };
            if json && content.is_none() {
                repointing.configs.push(path.clone());
            }
            before.put(
                path,
                Some(content.unwrap_or_default().into_bytes().into()),
                Path::is_dir,
            );
        } else if added.contains(&path) {
            before.put(path, None, Path::is_dir);
        } else if json {
            match earlier {
                // A file with no earlier version is one this change added.
                Some(earlier) => {
                    let content = earlier.text(&path).map(|text| text.into_bytes().into());
                    before.put(path, content, Path::is_dir);
                }
                None if is_config(&path) => repointing.configs.push(path),
                None => {}
            }
        }
    }
    repointing.before = Arc::new(before);
    repointing
}

/// A JSON file that can change how an import resolves when its earlier version is
/// not known. A `package.json` can too, but taking every change to one as moving
/// every import beneath it would report most changes to most repositories.
fn is_config(path: &Path) -> bool {
    !path
        .file_name()
        .is_some_and(|name| name == "package.json" || name == "package-lock.json")
}

/// Which imports the change may have moved, as one resolver resolves them.
///
/// Each import is resolved in the tree as it is and in the tree before the change,
/// by the same rules, and has moved when the two answer differently. Both answers
/// are the trees' own: a package the lockfile changed standing in for its files,
/// and the report of what could not be placed, belong to resolving an import for
/// the graph, and would make two different files look the same here.
///
/// One per resolver, because the answer is the resolver's: another bundler may
/// resolve the same file otherwise.
pub struct MovedImports<Fs = FileSystemOs> {
    repointing: Arc<Repointing>,
    /// The tree as it is, shared with the resolver.
    now: Arc<Tree<Fs>>,
    /// The tree as it was before the change, which only this asks about, and never
    /// on behalf of the unresolved report.
    before: Tree<BeforeFs<Fs>>,
    /// Where the `tsconfig.json` files above a file stop being looked for.
    root: PathBuf,
    /// The configuration files each tsconfig reads, itself first.
    tsconfig_reads: RwLock<AHashMap<PathBuf, Arc<[PathBuf]>>>,
    /// Finds the config a package `extends` entry names in the tree as it is, built
    /// on first use.
    extends: OnceLock<ResolverGeneric<Fs>>,
    /// The same in the tree before the change.
    extends_before: OnceLock<ResolverGeneric<BeforeFs<Fs>>>,
    /// Which imports of each file moved, by index, worked out the first time the
    /// file is asked about.
    indices: RwLock<AHashMap<PathBuf, Arc<[usize]>>>,
}

impl<Fs: FileSystem + Clone + 'static> MovedImports<Fs> {
    pub fn new(repointing: Arc<Repointing>, now: Arc<Tree<Fs>>, root: PathBuf) -> Self {
        let before = now.over(repointing.over(now.fs().clone()));
        Self {
            repointing,
            now,
            before,
            root,
            tsconfig_reads: RwLock::new(AHashMap::default()),
            extends: OnceLock::new(),
            extends_before: OnceLock::new(),
            indices: RwLock::new(AHashMap::default()),
        }
    }

    /// Which of `specifiers`, imported from `file`, the change may have sent to
    /// another file, by index. `specifiers` are read only the first time `file` is
    /// asked about, and only if the change could have moved anything.
    ///
    /// An import has moved when the tree before the change resolves it to another
    /// file, or to none, or to one where there is none now. Every import of a file
    /// a changed config with no earlier text may govern has moved, which is asked
    /// first: the tree before is no different from the one on disk there, and
    /// comparing the two would find nothing.
    pub fn moved<S: AsRef<[String]>>(
        &self,
        file: &Path,
        specifiers: impl FnOnce() -> S,
    ) -> Arc<[usize]> {
        if self.repointing.is_empty() {
            return Arc::from([]);
        }
        if let Some(known) = self.indices.read().unwrap().get(file) {
            return known.clone();
        }
        let specifiers = specifiers();
        let specifiers = specifiers.as_ref();
        let moved: Arc<[usize]> = if self.governed_by_unknown_config(file) {
            (0..specifiers.len()).collect()
        } else if !self.repointing.has_before() {
            Arc::from([])
        } else {
            specifiers
                .iter()
                .enumerate()
                .filter(|(_, specifier)| {
                    self.now.find(file, specifier).path()
                        != self.before.find(file, specifier).path()
                })
                .map(|(index, _)| index)
                .collect()
        };
        self.indices
            .write()
            .unwrap()
            .insert(file.to_path_buf(), moved.clone());
        moved
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
        match self.now.tsconfig_for(file) {
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
            if self
                .now
                .fs()
                .metadata(&above)
                .is_ok_and(|found| found.is_file())
            {
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
            let Some(parsed) =
                self.now.fs().read_to_string(&config).ok().and_then(|text| {
                    oxc_resolver::TsConfig::parse(true, &config, &config, text).ok()
                })
            else {
                continue;
            };
            let extends = match &parsed.extends {
                Some(oxc_resolver::ExtendsField::Single(one)) => vec![one.clone()],
                Some(oxc_resolver::ExtendsField::Multiple(many)) => many.clone(),
                None => Vec::new(),
            };
            let referenced = parsed.references.iter().filter_map(|reference| {
                let path = normalize(&config.parent()?.join(&reference.path));
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
                .chain(referenced.map(|path| self.canonical(path)))
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
            let path = normalize(&directory.join(specifier));
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
            return named.into_iter().map(|path| self.canonical(path)).collect();
        }
        // A package's config, looked for in both trees. One the change deleted is
        // found only in the tree before, and one it added only in the tree as it
        // is, and either may be the config whose earlier version is not known.
        let now = self.extends.get_or_init(|| {
            ResolverGeneric::new_with_file_system(self.now.fs().clone(), extends_options())
        });
        let before = self.extends_before.get_or_init(|| {
            ResolverGeneric::new_with_file_system(self.before.fs().clone(), extends_options())
        });
        let mut named: Vec<PathBuf> = Vec::new();
        let found = [
            now.resolve(directory, specifier),
            before.resolve(directory, specifier),
        ];
        for resolution in found.into_iter().flatten() {
            let path = resolution.full_path();
            if !named.contains(&path) {
                named.push(path);
            }
        }
        named
    }

    /// `path` as the tree as it is spells it, or as written where it is not there.
    fn canonical(&self, path: PathBuf) -> PathBuf {
        self.now
            .fs()
            .canonicalize(&path)
            .map(|real| dunce::simplified(&real).to_path_buf())
            .unwrap_or(path)
    }
}

/// How a package's config is looked for: the way oxc_resolver looks for it when it
/// follows `extends`, as a JSON file, and as a package's `tsconfig.json` where the
/// entry names only the package. A module resolver would find the package's code
/// instead.
fn extends_options() -> ResolveOptions {
    ResolveOptions {
        condition_names: vec!["node".to_string(), "import".to_string()],
        extensions: vec![".json".to_string()],
        main_files: vec!["tsconfig".to_string()],
        ..ResolveOptions::default()
    }
}

/// The tree before the change, for a resolver to look at: the tree as it is, with
/// what the change made a difference to laid over it.
#[derive(Clone)]
pub struct BeforeFs<Fs = FileSystemOs> {
    before: Arc<Before>,
    base: Fs,
}

/// What the tree before the change has at a path the change made a difference to.
enum Entry<'a> {
    File(&'a [u8]),
    Directory,
    Missing,
}

impl<Fs: FileSystem> BeforeFs<Fs> {
    /// What was at `path`, if the change made a difference there.
    ///
    /// A path is recorded as its canonical form, and a resolver may ask by another
    /// one: a workspace package is reached through the symlink `node_modules` holds
    /// for it. A path the disk does not have is looked up again by its canonical
    /// form, as far as the disk has one. Only a path the disk does not have needs
    /// asking twice, since the change made a difference only where it removed or
    /// added something.
    fn entry(&self, path: &Path) -> Option<Entry<'_>> {
        let look = |path: &Path| match self.before.files.get(path) {
            Some(Some(content)) => Some(Entry::File(content)),
            Some(None) => Some(Entry::Missing),
            None if self.before.dirs.contains(path) => Some(Entry::Directory),
            None => None,
        };
        if let Some(entry) = look(path) {
            return Some(entry);
        }
        if self.base.symlink_metadata(path).is_ok() {
            return None;
        }
        look(&canonical_in(&self.base, path))
    }
}

fn not_found() -> io::Error {
    io::Error::from(io::ErrorKind::NotFound)
}

impl<Fs: FileSystem> FileSystem for BeforeFs<Fs> {
    fn new() -> Self {
        Self {
            before: Arc::default(),
            base: Fs::new(),
        }
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        match self.entry(path) {
            Some(Entry::File(content)) => Ok(content.to_vec()),
            Some(_) => Err(not_found()),
            None => self.base.read(path),
        }
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        match self.entry(path) {
            Some(Entry::File(content)) => String::from_utf8(content.to_vec())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Some(_) => Err(not_found()),
            None => self.base.read_to_string(path),
        }
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        match self.entry(path) {
            Some(Entry::File(_)) => Ok(FileMetadata::new(true, false, false)),
            Some(Entry::Directory) => Ok(FileMetadata::new(false, true, false)),
            Some(Entry::Missing) => Err(not_found()),
            None => self.base.metadata(path),
        }
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        match self.entry(path) {
            Some(_) => self.metadata(path),
            None => self.base.symlink_metadata(path),
        }
    }

    fn read_link(&self, path: &Path) -> Result<PathBuf, ResolveError> {
        self.base.read_link(path)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        match self.base.canonicalize(path) {
            Ok(real) => Ok(real),
            // A path that is only in the tree before the change is spelled the way
            // it was recorded, which is canonical.
            Err(error) => match self.entry(path) {
                Some(Entry::File(_) | Entry::Directory) => Ok(canonical_in(&self.base, path)),
                _ => Err(error),
            },
        }
    }
}

/// `path` as the resolver spells it, whether or not it is still there: the nearest
/// directory above it that is, canonicalised, and the rest as written. Where the
/// working directory is spelled another way than its canonical path, as a short
/// name on Windows can be, a deleted file named from it would otherwise match no
/// path the resolver produces.
fn canonical(path: &Path) -> PathBuf {
    canonical_in(&FileSystemOs::new(), path)
}

/// [`canonical`], as `fs` has it. Spelled without the `\\?\` prefix Windows adds,
/// as oxc_resolver spells every path it returns.
fn canonical_in(fs: &impl FileSystem, path: &Path) -> PathBuf {
    let path = normalize(path);
    for ancestor in path.ancestors() {
        let real = fs
            .canonicalize(ancestor)
            .map(|real| dunce::simplified(&real).to_path_buf());
        if let (Ok(real), Ok(rest)) = (real, path.strip_prefix(ancestor)) {
            return if rest.as_os_str().is_empty() {
                real
            } else {
                real.join(rest)
            };
        }
    }
    path
}

/// Resolves `.` and `..` without asking the file system, which a deleted file is
/// no longer on.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_fs::{MemoryFs, at};

    #[test]
    fn the_tree_before_has_what_was_deleted_and_not_what_was_added() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/new.ts"), "").unwrap();
        let changes = crate::diff::parse(
            "diff --git a/src/new.ts b/src/new.ts\n\
             new file mode 100644\n\
             --- /dev/null\n\
             +++ b/src/new.ts\n\
             @@ -0,0 +1 @@\n\
             +x\n\
             diff --git a/src/shims/toolkit.ts b/src/shims/toolkit.ts\n\
             deleted file mode 100644\n\
             --- a/src/shims/toolkit.ts\n\
             +++ /dev/null\n\
             @@ -1 +0,0 @@\n\
             -x\n",
        );
        let repointing = repointing(&root, &changes, &[], None);
        let fs = repointing.over(FileSystemOs::new());
        let gone = root.join("src/shims/toolkit.ts");
        assert!(fs.metadata(&gone).unwrap().is_file());
        assert!(fs.metadata(&root.join("src/shims")).unwrap().is_dir());
        assert_eq!(fs.canonicalize(&gone).unwrap(), gone);
        assert!(fs.metadata(&root.join("src/new.ts")).is_err());
        assert!(fs.metadata(&root.join("src")).unwrap().is_dir());
    }

    #[test]
    fn a_renamed_file_is_still_at_its_old_path_before() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("src/legacy")).unwrap();
        std::fs::write(root.join("src/legacy/toolkit.ts"), "").unwrap();
        let changes = crate::diff::parse(
            "diff --git a/src/shims/toolkit.ts b/src/legacy/toolkit.ts\n\
             similarity index 100%\n\
             rename from src/shims/toolkit.ts\n\
             rename to src/legacy/toolkit.ts\n",
        );
        let fs = repointing(&root, &changes, &[], None).over(FileSystemOs::new());
        assert!(
            fs.metadata(&root.join("src/shims/toolkit.ts"))
                .unwrap()
                .is_file()
        );
        assert!(
            fs.metadata(&root.join("src/legacy/toolkit.ts"))
                .unwrap()
                .is_file()
        );
    }

    /// A project on disk, and the moved imports a diff against it finds with no base
    /// revision.
    fn project(files: &[(&str, &str)], diff: &str) -> (tempfile::TempDir, PathBuf, MovedImports) {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let repointing = repointing(&root, &crate::diff::parse(diff), &[], None);
        let now = Arc::new(Tree::new(
            Arc::new(crate::config::Configs::new(&root)),
            crate::config::Lookup::default(),
            FileSystemOs::new(),
        ));
        let moved = MovedImports::new(Arc::new(repointing), now, root.clone());
        (dir, root, moved)
    }

    fn specifiers(written: &[&str]) -> Vec<String> {
        written
            .iter()
            .map(|specifier| specifier.to_string())
            .collect()
    }

    /// What a change did to a tree held in memory: the files it deleted, with what
    /// they held, and the configs it changed with no earlier text to compare with.
    fn changed(now: &MemoryFs, deleted: &[(&str, &str)], unknown: &[&str]) -> Arc<Repointing> {
        let mut before = Before::default();
        let is_dir = |path: &Path| now.metadata(path).is_ok_and(|found| found.is_dir());
        for (path, text) in deleted {
            before.put(at(path), Some(text.as_bytes().into()), is_dir);
        }
        Arc::new(Repointing {
            before: Arc::new(before),
            configs: unknown.iter().map(|path| at(path)).collect(),
        })
    }

    /// The moved imports of a change to a tree held in memory.
    fn in_memory(now: MemoryFs, change: Arc<Repointing>) -> MovedImports<MemoryFs> {
        let now = Arc::new(Tree::new(
            Arc::new(crate::config::Configs::new(&at(""))),
            crate::config::Lookup::default(),
            now,
        ));
        MovedImports::new(change, now, at(""))
    }

    #[test]
    fn a_file_deleted_from_under_a_paths_mapping_moves_the_import() {
        let now = MemoryFs::with(&[
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "paths": { "@reduxjs/toolkit": ["./src/shims/toolkit.ts"] } } }"#,
            ),
            ("src/page.ts", ""),
        ]);
        let change = changed(&now, &[("src/shims/toolkit.ts", "")], &[]);
        let moved = in_memory(now, change);
        let imports = || specifiers(&["@reduxjs/toolkit", "./never-there"]);
        assert_eq!(&*moved.moved(&at("src/page.ts"), imports), &[0]);
    }

    /// Whether `config`, changed with no earlier text, is taken to govern
    /// `src/page.ts` in `now`: every import of a file it governs moves, and no
    /// import of one it does not, since nothing else changed.
    fn governs(now: &[(&str, &str)], deleted: &[(&str, &str)], config: &str) -> bool {
        let now = MemoryFs::with(now);
        let change = changed(&now, deleted, &[config]);
        let moved =
            in_memory(now, change).moved(&at("src/page.ts"), || specifiers(&["./a", "./b"]));
        match &*moved {
            [0, 1] => true,
            [] => false,
            other => panic!("a governed file moves every import, not {other:?}"),
        }
    }

    #[test]
    fn a_config_the_tsconfig_found_extends_governs_the_file() {
        let now = [
            ("tsconfig.json", r#"{ "extends": "./tsconfig.base.json" }"#),
            ("tsconfig.base.json", "{}"),
            ("src/page.ts", ""),
        ];
        assert!(governs(&now, &[], "tsconfig.base.json"));
    }

    /// A `tsconfig.json` above the file can decide through `references`, `include`
    /// or `exclude` which config is found, so what it reads governs the file too,
    /// though another config is the one found.
    #[test]
    fn a_config_a_tsconfig_above_extends_governs_the_file() {
        let now = [
            ("tsconfig.json", r#"{ "extends": "./tsconfig.base.json" }"#),
            ("tsconfig.base.json", "{}"),
            ("src/tsconfig.json", "{}"),
            ("src/page.ts", ""),
        ];
        assert!(governs(&now, &[], "tsconfig.base.json"));
    }

    /// A package's config that the change deleted is not there to be found in the
    /// tree as it is, and was in the tree before.
    #[test]
    fn a_deleted_package_config_a_tsconfig_above_extends_governs_the_file() {
        let now = [
            (
                "tsconfig.json",
                r#"{ "extends": "@acme/tsconfig/base.json" }"#,
            ),
            ("src/tsconfig.json", "{}"),
            ("src/page.ts", ""),
            (
                "node_modules/@acme/tsconfig/package.json",
                r#"{ "name": "@acme/tsconfig" }"#,
            ),
        ];
        let deleted = [("node_modules/@acme/tsconfig/base.json", "")];
        assert!(governs(
            &now,
            &deleted,
            "node_modules/@acme/tsconfig/base.json"
        ));
    }

    #[test]
    fn a_config_a_tsconfig_above_references_governs_the_file() {
        let now = [
            (
                "tsconfig.json",
                r#"{ "references": [{ "path": "./packages/lib" }] }"#,
            ),
            ("packages/lib/tsconfig.json", "{}"),
            ("src/page.ts", ""),
        ];
        assert!(governs(&now, &[], "packages/lib/tsconfig.json"));
    }

    /// It may have been the nearest before, whatever the tree as it is reads.
    #[test]
    fn a_deleted_tsconfig_above_the_file_governs_it() {
        let now = [("tsconfig.json", "{}"), ("src/page.ts", "")];
        let deleted = [("src/tsconfig.json", "")];
        assert!(governs(&now, &deleted, "src/tsconfig.json"));
    }

    /// What a tsconfig that cannot be read said before is exactly what is not known.
    #[test]
    fn a_tsconfig_that_cannot_be_read_governs_the_file() {
        let now = [
            ("tsconfig.json", "{ not json"),
            ("tools/tsconfig.json", "{}"),
            ("src/page.ts", ""),
        ];
        assert!(governs(&now, &[], "tools/tsconfig.json"));
    }

    #[test]
    fn an_unrelated_config_does_not_govern_the_file() {
        let now = [
            ("tsconfig.json", "{}"),
            ("tools/tsconfig.json", "{}"),
            ("src/page.ts", ""),
        ];
        assert!(!governs(&now, &[], "tools/tsconfig.json"));
    }

    /// With no base revision a changed tsconfig has no earlier text, so the tree
    /// before is the one on disk. The file it governs moves every import all the
    /// same, which it can only do if governance is asked before anything looks at
    /// whether the trees differ.
    #[test]
    fn an_unknown_config_moves_every_import_though_the_tree_before_is_the_same() {
        let (_dir, root, moved) = project(
            &[
                ("tsconfig.json", "{}"),
                ("src/page.ts", "import './a';\nimport './b';\n"),
                ("src/a.ts", ""),
                ("src/b.ts", ""),
            ],
            "diff --git a/tsconfig.json b/tsconfig.json\n\
             --- a/tsconfig.json\n\
             +++ b/tsconfig.json\n\
             @@ -1 +1 @@\n\
             -{ }\n\
             +{}\n",
        );
        let page = root.join("src/page.ts");
        assert_eq!(
            &*moved.moved(&page, || specifiers(&["./a", "./b"])),
            &[0, 1]
        );
    }

    /// The answer is the resolver's for the file, so a search that meets the file
    /// again asks nothing, and does not read its imports again either.
    #[test]
    fn a_file_asked_about_twice_is_resolved_once() {
        let (_dir, root, moved) = project(
            &[("src/page.ts", "import './gone';\n")],
            "diff --git a/src/gone.ts b/src/gone.ts\n\
             deleted file mode 100644\n\
             --- a/src/gone.ts\n\
             +++ /dev/null\n\
             @@ -1 +0,0 @@\n\
             -x\n",
        );
        let page = root.join("src/page.ts");
        assert_eq!(&*moved.moved(&page, || specifiers(&["./gone"])), &[0]);
        let again = moved.moved(&page, || -> Vec<String> {
            panic!("the imports were read again")
        });
        assert_eq!(&*again, &[0]);
    }

    #[test]
    fn a_deleted_file_is_spelled_the_way_the_resolver_spells_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let real = dunce::canonicalize(dir.path()).unwrap();
        let gone = dir.path().join("src/shims/../shims/toolkit.ts");
        assert_eq!(
            canonical(&gone),
            real.join("src").join("shims").join("toolkit.ts")
        );
    }
}
