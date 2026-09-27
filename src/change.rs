//! Everything a run takes the change to have done.
//!
//! A change reaches a run by three routes: files it edited, packages whose lockfile
//! entries it touched, and imports it may have moved without editing them. Each is
//! read from the same inputs — a diff, the paths named beside it, and the base
//! revision where there is one — and every search asks about all three, so they are
//! read once, here, with every path spelled one way.
//!
//! What this says about a file is its [`Extent`]: how much of it the change reaches,
//! in terms of the file alone. Turning that into graph nodes is [`crate::marks`]'s
//! work. Keeping change (what the inputs say) apart from granularity (what the
//! analysis can prove) is what lets later steps narrow without revisiting this code.

use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ahash::{AHashMap, AHashSet};

use crate::base::Earlier;
use crate::diff::{ChangeSet, FileChange, LineRange};
use crate::lockfile;
use crate::module::Reading;
use crate::module::compare::{self, Comparison};
use crate::repoint::Repointing;
use crate::resolve::Resolver;

/// How much of one file the change reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extent {
    /// Nothing a page could see: a reformatting, a reworded comment, a change made
    /// only of types in a run that ignores them.
    Unchanged,
    /// All of it, as far as anything can say.
    Whole,
    /// These lines of the current version, and roughly what sits on them.
    Lines(Vec<LineRange>),
    /// Exactly these statements, as the base version tells them apart.
    Statements(Comparison),
}

/// The change, as a run asks about it.
pub struct Change {
    root: PathBuf,
    diff: ChangeSet,
    /// The paths named beside the diff, joined to the root where relative.
    named: Vec<PathBuf>,
    earlier: Option<Box<dyn Earlier>>,
    /// The earlier version of a module is analysed like any other, and has to be:
    /// two versions read under different rules cannot be compared.
    reading: Reading,
    /// Worked out when first asked, since a run that searches nothing reads nothing.
    extents: OnceCell<Extents>,
    /// Packages whose lockfile entries changed. A resolver needs these to give each
    /// its node, so they are read before any search.
    packages: Arc<lockfile::Changed>,
    /// What the change could have sent an unchanged import to instead.
    repointing: Arc<Repointing>,
}

#[derive(Default)]
struct Extents {
    by_path: AHashMap<PathBuf, Extent>,
    /// Every path whose extent is not [`Extent::Unchanged`].
    files: AHashSet<PathBuf>,
}

impl Change {
    /// Reads the change from a diff and the paths named beside it.
    ///
    /// A relative path is relative to `root`, as a diff's paths are and as `git diff
    /// --name-only` writes them.
    pub fn read(
        root: &Path,
        diff: ChangeSet,
        named: &[PathBuf],
        earlier: Option<Box<dyn Earlier>>,
        reading: Reading,
    ) -> Self {
        let named = named
            .iter()
            .map(|path| {
                if path.is_relative() {
                    root.join(path)
                } else {
                    path.clone()
                }
            })
            .collect::<Vec<_>>();
        let packages = Arc::new(lockfile::changed(root, &diff, &named));
        let repointing = Arc::new(crate::repoint::repointing(
            root,
            &diff,
            &named,
            earlier.as_deref(),
        ));
        Self {
            root: root.to_path_buf(),
            diff,
            named,
            earlier,
            reading,
            extents: OnceCell::new(),
            packages,
            repointing,
        }
    }

    /// Whether the change reaches nothing at all: no file, no package, and no import
    /// it could have moved.
    pub fn is_empty(&self) -> bool {
        self.files().is_empty() && self.packages.is_empty() && self.repointing.is_empty()
    }

    /// Whether `path` is the node standing for a package the change reaches.
    pub fn marks_package(&self, path: &Path) -> bool {
        self.packages.marks(&self.root, path)
    }

    /// Whether `file`, which imports `specifiers`, imports something the change may
    /// have moved, as `resolver` resolves it. The specifiers are read only the first
    /// time `resolver` is asked about a file.
    pub fn repoints<S: AsRef<[String]>>(
        &self,
        resolver: &Resolver,
        file: &Path,
        specifiers: impl FnOnce() -> S,
    ) -> bool {
        if self.repointing.is_empty() {
            return false;
        }
        resolver.repoints_of(file, || {
            !resolver.moved(file, specifiers().as_ref()).is_empty()
        })
    }

    /// What a resolver needs of the change to resolve as the run does: which packages
    /// stand as nodes of their own, and what the tree was before.
    pub fn for_resolution(&self) -> (Arc<lockfile::Changed>, Arc<Repointing>) {
        (self.packages.clone(), self.repointing.clone())
    }

    /// How much of `path` the change reaches, or `None` for a file it does not name,
    /// or names only to delete.
    pub fn extent(&self, path: &Path) -> Option<&Extent> {
        self.read_extents().by_path.get(path)
    }

    /// Every file the change names and reaches, and how far.
    pub fn extents(&self) -> impl Iterator<Item = (&Path, &Extent)> {
        self.read_extents()
            .by_path
            .iter()
            .map(|(path, extent)| (path.as_path(), extent))
    }

    /// Every file whose extent is more than [`Extent::Unchanged`], for the searches
    /// that work file by file.
    pub fn files(&self) -> &AHashSet<PathBuf> {
        &self.read_extents().files
    }

    fn read_extents(&self) -> &Extents {
        self.extents.get_or_init(|| {
            let mut extents = Extents::default();
            for file in &self.diff.files {
                let extent = match &file.change {
                    // A deleted file cannot be reached from an anchor.
                    FileChange::Deleted => continue,
                    FileChange::Modified { ranges } => Extent::Lines(ranges.clone()),
                    FileChange::Opaque => Extent::Whole,
                };
                let path = self.root.join(&file.path);
                let extent = match self.against_base(&path) {
                    Some(extent) => extent,
                    None if self.only_types(&path, &extent) => Extent::Unchanged,
                    None => extent,
                };
                extents.put(&path, extent);
            }
            // A path named without line information is read the way a binary diff
            // is, which keeps the coarser way of naming a change at least as loud.
            for path in &self.named {
                let extent = self.against_base(path).unwrap_or(Extent::Whole);
                extents.put(path, extent);
            }
            extents
        })
    }

    /// Whether every line `extent` names has nothing on it that runs.
    ///
    /// This is all a line range can prove without an earlier version to compare
    /// against: a line holding an annotation and a value both could have changed in
    /// either, so it counts.
    fn only_types(&self, path: &Path, extent: &Extent) -> bool {
        if !self.reading.ignore_types {
            return false;
        }
        let Extent::Lines(ranges) = extent else {
            return false;
        };
        let Some((_, line_table)) = crate::module::analyse(path, &self.reading) else {
            return false;
        };
        !ranges.is_empty()
            && ranges
                .iter()
                .all(|range| line_table.runs_nothing(range.start, range.len))
    }

    /// What the base version of `path` says changed in it, or `None` when there is
    /// no answer to be had — no base version, no version of this file in it, or two
    /// versions that cannot be compared — and the diff has to say.
    fn against_base(&self, path: &Path) -> Option<Extent> {
        let path = dunce::canonicalize(path).ok()?;
        let before = self.earlier.as_ref()?.text(&path)?;
        let comparison = compare::compare(&path, &before, &self.reading)?;
        Some(if comparison.is_empty() {
            Extent::Unchanged
        } else if comparison.whole_file {
            Extent::Whole
        } else {
            Extent::Statements(comparison)
        })
    }
}

impl Extents {
    /// A path that is not there is dropped: it cannot be compared against resolved
    /// imports.
    fn put(&mut self, path: &Path, extent: Extent) {
        let Ok(path) = dunce::canonicalize(path) else {
            return;
        };
        // A file named twice is reached as far as either says.
        let extent = match self.by_path.remove(&path) {
            Some(Extent::Whole) => Extent::Whole,
            Some(_) if extent == Extent::Whole => Extent::Whole,
            Some(earlier) if extent == Extent::Unchanged => earlier,
            _ => extent,
        };
        if extent != Extent::Unchanged {
            self.files.insert(path.clone());
        }
        self.by_path.insert(path, extent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::ChangedFile;

    fn project(files: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        for file in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "export const x = 1;\n").unwrap();
        }
        (dir, root)
    }

    fn diff(files: &[(&str, FileChange)]) -> ChangeSet {
        ChangeSet {
            files: files
                .iter()
                .map(|(path, change)| ChangedFile {
                    path: PathBuf::from(path),
                    change: change.clone(),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn reading(root: &Path) -> Reading {
        Reading {
            configs: std::sync::Arc::new(crate::config::Configs::new(root)),
            ignore_types: true,
        }
    }

    #[test]
    fn a_diff_reaches_what_it_names_and_not_what_it_deletes_or_cannot_find() {
        let (_dir, root) = project(&["src/edited.ts", "src/binary.png"]);
        let lines = vec![LineRange { start: 1, len: 1 }];
        let change = Change::read(
            &root,
            diff(&[
                (
                    "src/edited.ts",
                    FileChange::Modified {
                        ranges: lines.clone(),
                    },
                ),
                ("src/binary.png", FileChange::Opaque),
                ("src/gone.ts", FileChange::Deleted),
                ("src/never-existed.ts", FileChange::Opaque),
            ]),
            &[],
            None,
            reading(&root),
        );

        assert_eq!(
            change.extent(&root.join("src/edited.ts")),
            Some(&Extent::Lines(lines))
        );
        assert_eq!(
            change.extent(&root.join("src/binary.png")),
            Some(&Extent::Whole)
        );
        assert_eq!(change.extent(&root.join("src/gone.ts")), None);
        assert_eq!(change.extent(&root.join("src/never-existed.ts")), None);
        assert_eq!(change.files().len(), 2);
    }

    /// The tests run from the crate's directory, never from the project's, so a path
    /// read against the working directory would name nothing here.
    #[test]
    fn a_named_path_reaches_the_whole_file_and_is_read_against_the_root() {
        let (_dir, root) = project(&["src/relative.ts", "src/absolute.ts"]);
        let change = Change::read(
            &root,
            ChangeSet::default(),
            &[
                PathBuf::from("src/relative.ts"),
                root.join("src/absolute.ts"),
                PathBuf::from("src/never-existed.ts"),
            ],
            None,
            reading(&root),
        );

        assert_eq!(
            change.extent(&root.join("src/relative.ts")),
            Some(&Extent::Whole)
        );
        assert_eq!(
            change.extent(&root.join("src/absolute.ts")),
            Some(&Extent::Whole)
        );
        assert_eq!(change.files().len(), 2);
    }

    #[test]
    fn a_diff_and_a_named_path_for_the_same_file_reach_the_whole_of_it() {
        let (_dir, root) = project(&["src/a.ts"]);
        let change = Change::read(
            &root,
            diff(&[(
                "src/a.ts",
                FileChange::Modified {
                    ranges: vec![LineRange { start: 1, len: 1 }],
                },
            )]),
            &[PathBuf::from("src/a.ts")],
            None,
            reading(&root),
        );

        assert_eq!(change.extent(&root.join("src/a.ts")), Some(&Extent::Whole));
    }

    fn earlier(root: &Path, files: &[(&str, &str)]) -> Box<dyn Earlier> {
        Box::new(
            files
                .iter()
                .map(|(path, text)| (root.join(path), text.to_string()))
                .collect::<AHashMap<_, _>>(),
        )
    }

    #[test]
    fn the_base_version_decides_what_changed_where_there_is_one() {
        let (_dir, root) = project(&["src/reformatted.ts", "src/edited.ts", "src/new.ts"]);
        let lines = vec![LineRange { start: 1, len: 1 }];
        let modified = || FileChange::Modified {
            ranges: lines.clone(),
        };
        let change = Change::read(
            &root,
            diff(&[
                ("src/reformatted.ts", modified()),
                ("src/edited.ts", modified()),
                ("src/new.ts", modified()),
            ]),
            &[],
            Some(earlier(
                &root,
                &[
                    ("src/reformatted.ts", "export const x=1"),
                    ("src/edited.ts", "export const x = 2;\n"),
                ],
            )),
            reading(&root),
        );

        assert_eq!(
            change.extent(&root.join("src/reformatted.ts")),
            Some(&Extent::Unchanged)
        );
        assert!(matches!(
            change.extent(&root.join("src/edited.ts")),
            Some(Extent::Statements(comparison)) if comparison.changed.len() == 1
        ));
        assert_eq!(
            change.extent(&root.join("src/new.ts")),
            Some(&Extent::Lines(lines))
        );
        assert_eq!(change.files().len(), 2);
    }

    #[test]
    fn the_base_version_answers_for_a_named_path_too() {
        let (_dir, root) = project(&["src/reformatted.ts"]);
        let change = Change::read(
            &root,
            ChangeSet::default(),
            &[PathBuf::from("src/reformatted.ts")],
            Some(earlier(
                &root,
                &[("src/reformatted.ts", "export const x=1")],
            )),
            reading(&root),
        );

        assert_eq!(
            change.extent(&root.join("src/reformatted.ts")),
            Some(&Extent::Unchanged)
        );
        assert!(change.files().is_empty());
    }

    #[test]
    fn lines_holding_only_types_reach_nothing_when_types_are_ignored() {
        let (_dir, root) = project(&[]);
        std::fs::write(
            root.join("typed.ts"),
            "export type Id = string;\nexport const x = 1;\n",
        )
        .unwrap();
        let types_only = vec![LineRange { start: 1, len: 1 }];
        let read = |ignore_types| {
            Change::read(
                &root,
                diff(&[(
                    "typed.ts",
                    FileChange::Modified {
                        ranges: types_only.clone(),
                    },
                )]),
                &[],
                None,
                Reading {
                    ignore_types,
                    ..reading(&root)
                },
            )
        };

        assert_eq!(
            read(true).extent(&root.join("typed.ts")),
            Some(&Extent::Unchanged)
        );
        assert!(read(true).files().is_empty());
        assert_eq!(
            read(false).extent(&root.join("typed.ts")),
            Some(&Extent::Lines(types_only.clone()))
        );
    }

    #[test]
    fn nothing_named_is_no_change() {
        let (_dir, root) = project(&[]);
        let change = Change::read(&root, ChangeSet::default(), &[], None, reading(&root));
        assert!(change.is_empty());
    }

    #[test]
    fn a_named_lockfile_changes_the_packages_it_holds() {
        let (_dir, root) = project(&[]);
        std::fs::write(
            root.join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n\npackages:\n\n  react@18.3.1:\n    resolution: {integrity: sha512-aaa==}\n",
        )
        .unwrap();
        let change = Change::read(
            &root,
            ChangeSet::default(),
            &[PathBuf::from("pnpm-lock.yaml")],
            None,
            reading(&root),
        );

        assert!(!change.is_empty());
        assert!(change.marks_package(&root.join("node_modules/react")));
        assert!(!change.marks_package(&root.join("node_modules/vue")));
    }

    /// A path named beside the diff that is not there was deleted, and an import
    /// may have resolved to it.
    #[test]
    fn a_named_path_that_is_gone_may_have_moved_an_import() {
        let (_dir, root) = project(&[]);
        let change = Change::read(
            &root,
            ChangeSet::default(),
            &[PathBuf::from("src/shims/toolkit.ts")],
            None,
            reading(&root),
        );

        assert!(change.files().is_empty());
        assert!(!change.is_empty());
    }
}
