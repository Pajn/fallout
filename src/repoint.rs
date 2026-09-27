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
use std::sync::Arc;

use ahash::{AHashMap, AHashSet};
use oxc_resolver::{FileMetadata, FileSystem, FileSystemOs, ResolveError};

use crate::base::Base;
use crate::diff::{ChangeSet, FileChange};

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

    /// A file system that shows the tree before the change.
    pub fn file_system(&self) -> BeforeFs {
        BeforeFs {
            before: self.before.clone(),
            os: FileSystemOs::new(),
        }
    }

    /// Changed configs whose earlier version is not known.
    pub fn configs(&self) -> &[PathBuf] {
        &self.configs
    }
}

impl Before {
    fn put(&mut self, path: PathBuf, content: Option<Arc<[u8]>>) {
        if content.is_some() {
            for directory in path.ancestors().skip(1) {
                if directory.is_dir() || !self.dirs.insert(directory.to_path_buf()) {
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
    base: Option<&Base>,
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
        .chain(explicit.iter().map(|path| {
            let path = std::path::absolute(path).unwrap_or_else(|_| path.clone());
            let deleted = !path.exists();
            (path, deleted)
        }));

    let mut repointing = Repointing::default();
    let mut before = Before::default();
    let earlier = |path: &Path| base.and_then(|base| base.text(path));
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
            let content = if json { earlier(&path) } else { None };
            if json && content.is_none() {
                repointing.configs.push(path.clone());
            }
            before.put(path, Some(content.unwrap_or_default().into_bytes().into()));
        } else if added.contains(&path) {
            before.put(path, None);
        } else if json {
            match base {
                // A file with no earlier version is one this change added.
                Some(base) => {
                    let content = base.text(&path).map(|text| text.into_bytes().into());
                    before.put(path, content);
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

/// The tree before the change, for a resolver to look at.
pub struct BeforeFs {
    before: Arc<Before>,
    os: FileSystemOs,
}

/// What the tree before the change has at a path the change made a difference to.
enum Entry<'a> {
    File(&'a [u8]),
    Directory,
    Missing,
}

impl BeforeFs {
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
        if self.os.symlink_metadata(path).is_ok() {
            return None;
        }
        look(&canonical(path))
    }
}

fn not_found() -> io::Error {
    io::Error::from(io::ErrorKind::NotFound)
}

impl FileSystem for BeforeFs {
    fn new() -> Self {
        Self {
            before: Arc::default(),
            os: FileSystemOs::new(),
        }
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        match self.entry(path) {
            Some(Entry::File(content)) => Ok(content.to_vec()),
            Some(_) => Err(not_found()),
            None => self.os.read(path),
        }
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        match self.entry(path) {
            Some(Entry::File(content)) => String::from_utf8(content.to_vec())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Some(_) => Err(not_found()),
            None => self.os.read_to_string(path),
        }
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        match self.entry(path) {
            Some(Entry::File(_)) => Ok(FileMetadata::new(true, false, false)),
            Some(Entry::Directory) => Ok(FileMetadata::new(false, true, false)),
            Some(Entry::Missing) => Err(not_found()),
            None => self.os.metadata(path),
        }
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        match self.entry(path) {
            Some(_) => self.metadata(path),
            None => self.os.symlink_metadata(path),
        }
    }

    fn read_link(&self, path: &Path) -> Result<PathBuf, ResolveError> {
        self.os.read_link(path)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        match self.os.canonicalize(path) {
            Ok(real) => Ok(real),
            // A path that is only in the tree before the change is spelled the way
            // it was recorded, which is canonical.
            Err(error) => match self.entry(path) {
                Some(Entry::File(_) | Entry::Directory) => Ok(canonical(path)),
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
    let path = normalize(path);
    for ancestor in path.ancestors() {
        if let (Ok(real), Ok(rest)) = (dunce::canonicalize(ancestor), path.strip_prefix(ancestor)) {
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
        let fs = repointing.file_system();
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
        let fs = repointing(&root, &changes, &[], None).file_system();
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
