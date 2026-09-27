//! A file system held in memory, for tests that resolve without a disk.
//!
//! oxc_resolver ships none publicly. This one has files and the directories above
//! them, and nothing else: no symlinks, and every path is its own canonical
//! spelling. What turns on a link, or on how a directory is spelled, stays with the
//! tests that build a temporary directory.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ahash::{AHashMap, AHashSet};
use oxc_resolver::{FileMetadata, FileSystem, ResolveError};

/// Where every tree held in memory is rooted: a path that is absolute on every
/// platform the tests run on.
pub fn root() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\repo")
    } else {
        PathBuf::from("/repo")
    }
}

/// `relative`, written with `/`, beneath [`root`].
pub fn at(relative: &str) -> PathBuf {
    relative
        .split('/')
        .filter(|segment| !segment.is_empty())
        .fold(root(), |path, segment| path.join(segment))
}

#[derive(Clone, Default)]
pub struct MemoryFs {
    files: Arc<AHashMap<PathBuf, Vec<u8>>>,
    dirs: Arc<AHashSet<PathBuf>>,
}

impl MemoryFs {
    /// A tree holding `files`, each a path beneath [`root`] and its text.
    pub fn with(files: &[(&str, &str)]) -> Self {
        let files: AHashMap<PathBuf, Vec<u8>> = files
            .iter()
            .map(|(path, text)| (at(path), text.as_bytes().to_vec()))
            .collect();
        let dirs = files
            .keys()
            .flat_map(|path| path.ancestors().skip(1).map(Path::to_path_buf))
            .collect();
        Self {
            files: Arc::new(files),
            dirs: Arc::new(dirs),
        }
    }
}

fn not_found() -> io::Error {
    io::Error::from(io::ErrorKind::NotFound)
}

impl FileSystem for MemoryFs {
    fn new() -> Self {
        Self::default()
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.files.get(path).cloned().ok_or_else(not_found)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        String::from_utf8(self.read(path)?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        if self.files.contains_key(path) {
            Ok(FileMetadata::new(true, false, false))
        } else if self.dirs.contains(path) {
            Ok(FileMetadata::new(false, true, false))
        } else {
            Err(not_found())
        }
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        self.metadata(path)
    }

    fn read_link(&self, _path: &Path) -> Result<PathBuf, ResolveError> {
        Err(io::Error::from(io::ErrorKind::InvalidInput).into())
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        let path = crate::repoint::normalize(path);
        self.metadata(&path).map(|_| path)
    }
}
