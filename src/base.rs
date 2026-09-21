//! The version of a file a change is measured against.
//!
//! A diff says which lines moved; it cannot say whether the file means anything
//! different afterwards. With the earlier version of the file in hand, the two can
//! be compared as syntax rather than as text, so a reformatting, a reworded comment
//! or a statement that merely moved stops counting as a change at all.
//!
//! The earlier version is read from git, which is where a `--base` revision is
//! written down. A file git cannot produce — one that is new, or a revision it does
//! not know — simply has no base version, and the caller falls back to the diff.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;

use ahash::AHashMap;

use crate::module::compare::{self, Comparison};
use crate::pure::PureList;

/// A revision to compare against, plus the contents already read from it.
pub struct Base {
    reference: String,
    /// The base version of a module is analysed like any other, and that asks what
    /// the project calls pure.
    pure: PureList,
    contents: RefCell<AHashMap<PathBuf, Option<String>>>,
}

impl Base {
    pub fn new(reference: &str, pure: PureList) -> Self {
        Self {
            reference: reference.to_string(),
            pure,
            contents: RefCell::new(AHashMap::default()),
        }
    }

    /// How `path` differs from its base version, or `None` when there is no answer
    /// to be had: no version of it in this revision, or two versions that cannot be
    /// compared. The caller then falls back on what the diff says.
    pub fn comparison(&self, path: &Path) -> Option<Comparison> {
        compare::compare(path, &self.before(path)?, &self.pure)
    }

    /// What `path` contained at the base revision, or `None` when there is no such
    /// version to read.
    fn before(&self, path: &Path) -> Option<String> {
        if let Some(cached) = self.contents.borrow().get(path) {
            return cached.clone();
        }
        let text = self.read(path);
        self.contents
            .borrow_mut()
            .insert(path.to_path_buf(), text.clone());
        text
    }

    fn read(&self, path: &Path) -> Option<String> {
        // Naming the file relative to its own directory saves working out where the
        // repository root is, and works the same from a worktree or a subdirectory.
        let directory = path.parent()?;
        let name = path.file_name()?.to_str()?;

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
