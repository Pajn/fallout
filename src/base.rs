//! The version of a file a change is measured against.
//!
//! A diff says which lines moved; it cannot say whether the file means anything
//! different afterwards. With the earlier version of the file in hand, the two can
//! be compared as syntax rather than as text, so a reformatting, a reworded comment
//! or a statement that merely moved stops counting as a change at all.
//!
//! A run reads the earlier version through [`Earlier`], so that what it does with
//! one does not depend on where it came from. A run is given one as a `--base`
//! revision, read from git by [`Base`]. A file with no earlier version — one that is
//! new, or a revision git does not know — simply has none, and the caller falls back
//! to the diff.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;

use ahash::AHashMap;

/// Where the version of a file before the change comes from.
pub trait Earlier {
    /// What `path` contained before the change, or `None` when there is no such
    /// version to read.
    fn text(&self, path: &Path) -> Option<String>;
}

/// Earlier versions written down beforehand, by absolute path.
impl Earlier for AHashMap<PathBuf, String> {
    fn text(&self, path: &Path) -> Option<String> {
        self.get(path).cloned()
    }
}

/// A git revision to compare against, plus the contents already read from it.
pub struct Base {
    reference: String,
    contents: RefCell<AHashMap<PathBuf, Option<String>>>,
}

impl Base {
    pub fn new(reference: &str) -> Self {
        Self {
            reference: reference.to_string(),
            contents: RefCell::new(AHashMap::default()),
        }
    }
}

impl Earlier for Base {
    fn text(&self, path: &Path) -> Option<String> {
        if let Some(cached) = self.contents.borrow().get(path) {
            return cached.clone();
        }
        let text = self.read(path);
        self.contents
            .borrow_mut()
            .insert(path.to_path_buf(), text.clone());
        text
    }
}

impl Base {
    fn read(&self, path: &Path) -> Option<String> {
        // Naming the file relative to its own directory saves working out where the
        // repository root is, and works the same from a worktree or a subdirectory.
        // A deleted file's directory may have gone with it, so the nearest one that
        // is still there names it instead.
        let directory = path
            .ancestors()
            .skip(1)
            .find(|directory| directory.is_dir())?;
        let name = path
            .strip_prefix(directory)
            .ok()?
            .components()
            .map(|component| component.as_os_str().to_str())
            .collect::<Option<Vec<_>>>()?
            .join("/");

        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .arg("show")
            .arg(format!("{}:./{}", self.reference, name))
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }
        // A file git stores but we cannot decode as text is not one we could have
        // parsed either.
        String::from_utf8(output.stdout).ok()
    }
}
