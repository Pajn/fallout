//! The version of a file a change is measured against.
//!
//! A diff says which lines moved; it cannot say whether the file means anything
//! different afterwards. With the earlier version of the file in hand, the two can
//! be compared as syntax rather than as text, so a reformatting, a reworded comment
//! or a statement that merely moved stops counting as a change at all.
//!
//! A run reads the earlier version through [`Earlier`], so that what it does with
//! one does not depend on where it came from. A run is given one as a `--base`
//! revision, read from git by [`Base`]. A file with no earlier version, one that is
//! new, simply has none, and the caller falls back to the diff. A revision git does
//! not know would have none for any file, so a run refuses it before it begins: see
//! [`Base::is_known`].

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;

use ahash::AHashMap;

/// Where the version of a file before the change comes from.
pub trait Earlier {
    /// What `path` contained before the change, or `None` when there is no such
    /// version to read.
    ///
    /// `None` is read as "the file was not there before", not as "not known": a
    /// changed config with no earlier version is taken as one the change added, so
    /// the imports it governs are not taken as moved. An implementation has to
    /// answer for every text file of the tree before the change, or a run can
    /// report less than the change reaches.
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
    /// Whether git finds a commit by this revision from `root`, or from the nearest
    /// directory above it that is there, as [`Earlier::text`] asks for each file.
    ///
    /// A revision git cannot find has no version of any file, and [`Earlier::text`]
    /// cannot tell that apart from a file the change added. A run has to ask this
    /// first, and give no answer when it is false. Git that cannot be run, and a
    /// root outside any repository, find nothing either.
    pub fn is_known(&self, root: &Path) -> bool {
        let Some(directory) = root.ancestors().find(|directory| directory.is_dir()) else {
            return false;
        };
        Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("{}^{{commit}}", self.reference))
            .output()
            .is_ok_and(|output| output.status.success())
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Files and their contents, by path relative to the repository root.
    type Tree<'t> = &'t [(&'t str, &'t [u8])];

    /// A repository whose one commit holds `before`, with `after` checked out over
    /// the top: what a `--base HEAD` run reads.
    fn repository(before: Tree, after: Tree) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let repo = dunce::canonicalize(dir.path()).expect("canonical temp dir");
        write(&repo, before);
        git(&repo, &["init", "--quiet"]);
        git(&repo, &["add", "--all", "--force"]);
        git(
            &repo,
            &["commit", "--quiet", "--allow-empty", "--message", "before"],
        );
        for entry in std::fs::read_dir(&repo).expect("readable repository") {
            let path = entry.expect("readable entry").path();
            if path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }
            if path.is_dir() {
                std::fs::remove_dir_all(&path).expect("removing directory");
            } else {
                std::fs::remove_file(&path).expect("removing file");
            }
        }
        write(&repo, after);
        (dir, repo)
    }

    fn write(root: &Path, tree: Tree) {
        for (relative, content) in tree {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("directory");
            std::fs::write(path, content).expect("writing file");
        }
    }

    /// The repository stands alone: no identity, ignore list, hook or signing
    /// setting from anywhere else takes part in making its one commit.
    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(repo)
            .args(["-c", "user.name=fallout"])
            .args(["-c", "user.email=fallout@example.invalid"])
            .args(["-c", "commit.gpgsign=false"])
            .args(["-c", "core.excludesFile=/dev/null"])
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .status()
            .expect("running git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// The earlier versions of the files under `root`, written down the way a run
    /// names them: what a caller with no repository hands a run. A file that is not
    /// text has no version a run could read, so it is left out.
    fn written_down(root: &Path, before: &[(&str, &[u8])]) -> AHashMap<PathBuf, String> {
        before
            .iter()
            .filter_map(|(relative, content)| {
                let text = String::from_utf8(content.to_vec()).ok()?;
                Some((root.join(relative), text))
            })
            .collect()
    }

    /// Beneath `prefix`, and relative to it.
    fn below<'t>(tree: Tree<'t>, prefix: &str) -> Vec<(&'t str, &'t [u8])> {
        tree.iter()
            .filter_map(|(path, content)| {
                let relative = if prefix.is_empty() {
                    *path
                } else {
                    path.strip_prefix(prefix)?.strip_prefix('/')?
                };
                Some((relative, *content))
            })
            .collect()
    }

    /// One question put to an [`Earlier`], and the answer every adapter must give.
    struct Case {
        name: &'static str,
        /// Where the run is rooted, relative to the repository.
        root: &'static str,
        before: Tree<'static>,
        after: Tree<'static>,
        /// The file asked about, relative to the run's root.
        asked: &'static str,
        answer: Option<&'static str>,
    }

    const CASES: &[Case] = &[
        Case {
            name: "a file present in the base",
            root: "",
            before: &[("page.ts", b"export const page = 1;\n")],
            after: &[("page.ts", b"export const page = 2;\n")],
            asked: "page.ts",
            answer: Some("export const page = 1;\n"),
        },
        Case {
            name: "a file in a subdirectory",
            root: "",
            before: &[("src/lib/page.ts", b"export const page = 1;\n")],
            after: &[("src/lib/page.ts", b"export const page = 2;\n")],
            asked: "src/lib/page.ts",
            answer: Some("export const page = 1;\n"),
        },
        Case {
            name: "a deleted file whose directory survives",
            root: "",
            before: &[
                ("src/gone.ts", b"export const gone = 1;\n"),
                ("src/kept.ts", b"export const kept = 1;\n"),
            ],
            after: &[("src/kept.ts", b"export const kept = 1;\n")],
            asked: "src/gone.ts",
            answer: Some("export const gone = 1;\n"),
        },
        Case {
            name: "a deleted file whose directory is gone",
            root: "",
            before: &[
                ("src/old/deep/gone.ts", b"export const gone = 1;\n"),
                ("src/kept.ts", b"export const kept = 1;\n"),
            ],
            after: &[("src/kept.ts", b"export const kept = 1;\n")],
            asked: "src/old/deep/gone.ts",
            answer: Some("export const gone = 1;\n"),
        },
        Case {
            name: "an added file",
            root: "",
            before: &[("src/kept.ts", b"export const kept = 1;\n")],
            after: &[
                ("src/kept.ts", b"export const kept = 1;\n"),
                ("src/new/added.ts", b"export const added = 1;\n"),
            ],
            asked: "src/new/added.ts",
            answer: None,
        },
        Case {
            name: "a file that is not text",
            root: "",
            before: &[(
                "logo.png",
                &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0xff],
            )],
            after: &[(
                "logo.png",
                &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0xfe],
            )],
            asked: "logo.png",
            answer: None,
        },
        Case {
            name: "a root below the repository's, as for one app of a monorepo",
            root: "apps/web",
            before: &[
                ("apps/web/src/page.ts", b"export const page = 1;\n"),
                ("packages/ui/button.ts", b"export const button = 1;\n"),
            ],
            after: &[
                ("apps/web/src/page.ts", b"export const page = 2;\n"),
                ("packages/ui/button.ts", b"export const button = 1;\n"),
            ],
            asked: "src/page.ts",
            answer: Some("export const page = 1;\n"),
        },
    ];

    /// Every adapter answers every case alike, so a run given its earlier versions
    /// one way decides what it would have decided given them the other. The fixture
    /// harness hands its runs a map on the strength of this.
    #[test]
    fn every_earlier_gives_the_same_answers() {
        for case in CASES {
            let (_keep, repo) = repository(case.before, case.after);
            let root = if case.root.is_empty() {
                repo.clone()
            } else {
                repo.join(case.root)
            };
            let asked = root.join(case.asked);

            let adapters: [(&str, Box<dyn Earlier>); 2] = [
                ("git", Box::new(Base::new("HEAD"))),
                (
                    "map",
                    Box::new(written_down(&root, &below(case.before, case.root))),
                ),
            ];
            for (adapter, earlier) in adapters {
                assert_eq!(
                    earlier.text(&asked).as_deref(),
                    case.answer,
                    "{adapter}: {}",
                    case.name
                );
            }
        }
    }

    /// A revision git cannot find answers `None` for every file, which reads as a
    /// tree the change added whole. So a run is not given one: it asks first whether
    /// git finds a commit by that name from where the run is rooted.
    #[test]
    fn a_revision_is_known_only_when_git_finds_a_commit_by_it() {
        let (_keep, repo) = repository(
            &[("src/page.ts", b"export const page = 1;\n")],
            &[("src/page.ts", b"export const page = 2;\n")],
        );
        let known = |reference: &str, root: &Path| Base::new(reference).is_known(root);

        assert!(known("HEAD", &repo));
        assert!(known("HEAD", &repo.join("src")), "from a subdirectory");
        assert!(
            known("HEAD", &repo.join("src/gone/deeper")),
            "from a root that is not there, by the nearest directory that is"
        );
        assert!(!known("nosuchrev", &repo));
        assert!(!known("HEAD^{tree}", &repo), "a tree is not a commit");
    }
}
