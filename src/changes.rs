//! Turning a parsed diff plus any `--changed` paths into the set of marked files.
//!
//! At file granularity every change marks its whole file, so the line ranges a diff
//! carries are recorded but not yet consulted. Keeping change (what the diff says)
//! separate from granularity (what the analysis can prove) is what lets later steps
//! narrow without revisiting this code.

use std::path::{Path, PathBuf};

use ahash::AHashSet;

use crate::diff::{ChangeSet, FileChange};

/// Resolves everything a run considers changed to absolute paths.
///
/// Files with no after version are dropped: a deleted file cannot be reached from an
/// anchor, and a path that does not exist cannot be compared against resolved imports.
pub fn marked_files(root: &Path, diff: &ChangeSet, explicit: &[PathBuf]) -> AHashSet<PathBuf> {
    let mut marked = AHashSet::default();

    for file in &diff.files {
        if file.change == FileChange::Deleted {
            continue;
        }
        if let Ok(path) = root.join(&file.path).canonicalize() {
            marked.insert(path);
        }
    }

    for path in explicit {
        if let Ok(path) = path.canonicalize() {
            marked.insert(path);
        }
    }

    marked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::ChangedFile;

    #[test]
    fn drops_deleted_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("kept.ts"), "").unwrap();

        let diff = ChangeSet {
            files: vec![
                ChangedFile {
                    path: PathBuf::from("kept.ts"),
                    change: FileChange::Opaque,
                },
                ChangedFile {
                    path: PathBuf::from("gone.ts"),
                    change: FileChange::Deleted,
                },
            ],
        };

        let marked = marked_files(root, &diff, &[]);
        assert_eq!(marked.len(), 1);
        assert!(marked.contains(&root.join("kept.ts").canonicalize().unwrap()));
    }

    #[test]
    fn drops_paths_that_do_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let diff = ChangeSet {
            files: vec![ChangedFile {
                path: PathBuf::from("never-existed.ts"),
                change: FileChange::Opaque,
            }],
        };

        assert!(marked_files(dir.path(), &diff, &[]).is_empty());
    }

    #[test]
    fn unions_the_diff_with_explicit_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.ts"), "").unwrap();
        std::fs::write(root.join("b.ts"), "").unwrap();

        let diff = ChangeSet {
            files: vec![ChangedFile {
                path: PathBuf::from("a.ts"),
                change: FileChange::Opaque,
            }],
        };

        let marked = marked_files(root, &diff, &[root.join("b.ts")]);
        assert_eq!(marked.len(), 2);
    }
}
