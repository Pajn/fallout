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
//! [`Base::resolve`].

use std::cell::RefCell;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

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
    batch: RefCell<BatchState>,
}

enum BatchState {
    Pending,
    Ready(Batch),
    /// If this git cannot use the batch protocol, keep the original reader.
    Disabled,
}

/// One object reader for the run. Requests are still lazy, so only the files
/// needed by the change are read, but each does not start another git process.
struct Batch {
    root: PathBuf,
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Batch {
    /// Start a reader for the nearest checkout, leaving custom layouts to Git.
    fn open(directory: &Path) -> Option<Self> {
        // Locate a normal checkout or worktree by its .git directory or file,
        // without another git startup. Custom repository layouts retain the
        // original reader, which asks git itself where each path belongs.
        if std::env::var_os("GIT_DIR").is_some() || std::env::var_os("GIT_WORK_TREE").is_some() {
            return None;
        }
        let root = directory
            .ancestors()
            .find(|directory| directory.join(".git").exists())?
            .to_path_buf();
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&root)
            // NUL framing permits whitespace and newlines in file names. Blob
            // contents are framed by their byte length, not by their lines.
            .args(["cat-file", "--batch", "-Z"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let input = child.stdin.take().expect("piped stdin");
        let output = BufReader::new(child.stdout.take().expect("piped stdout"));
        Some(Self {
            root,
            child,
            input,
            output,
        })
    }

    /// Request one revision-relative path and consume its complete response.
    fn text(&mut self, reference: &str, name: &str) -> io::Result<Option<String>> {
        write!(self.input, "{reference}:{name}\0")?;
        self.input.flush()?;
        read_object(&mut self.output)
    }

    /// Whether this checkout owns the path, excluding nested repositories.
    fn contains(&self, path: &Path) -> bool {
        // The original reader asks the repository owning each path. A nested
        // checkout must not accidentally be read from this reader's repository.
        for directory in path.ancestors().skip(1) {
            if directory == self.root {
                return true;
            }
            if directory.join(".git").exists() {
                return false;
            }
        }
        false
    }
}

impl Drop for Batch {
    /// Stop and reap the child even after a failed request.
    fn drop(&mut self) {
        // Reap the reader on success and on a broken protocol alike. There is no
        // outstanding request at the end of a successful read.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Decode a NUL-framed blob, distinguishing absence from a broken protocol.
fn read_object(output: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut header = Vec::new();
    output.read_until(0, &mut header)?;
    if header.pop() != Some(0) {
        return Err(io::Error::other("unterminated git object header"));
    }
    if header.ends_with(b" missing") {
        return Ok(None);
    }
    let header = std::str::from_utf8(&header).map_err(io::Error::other)?;
    let mut fields = header.split_whitespace();
    let (_, Some("blob"), Some(size), None) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return Err(io::Error::other("unexpected git object header"));
    };
    let size = size.parse::<usize>().map_err(io::Error::other)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(io::Error::other)?;
    bytes.resize(size, 0);
    output.read_exact(&mut bytes)?;
    let mut end = [0];
    output.read_exact(&mut end)?;
    if end != [0] {
        return Err(io::Error::other("unterminated git object contents"));
    }
    // A non-text blob is absent to the analysis, but its whole response has been
    // consumed so the next request still starts at its own header.
    Ok(String::from_utf8(bytes).ok())
}

impl Base {
    /// Create a lazy reader for a revision, without validating it beforehand.
    pub fn new(reference: &str) -> Self {
        Self {
            reference: reference.to_string(),
            contents: RefCell::new(AHashMap::default()),
            batch: RefCell::new(BatchState::Pending),
        }
    }
}

impl Earlier for Base {
    /// Read and cache an earlier version, including absent and non-text files.
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
    /// The revision as the commit git finds by it from `root`, or from the nearest
    /// directory above it that is there, as [`Earlier::text`] asks for each file.
    ///
    /// A revision git cannot find has no version of any file, and [`Earlier::text`]
    /// cannot tell that apart from a file the change added. A run has to resolve it
    /// first, and give no answer when this is `None`. Git that cannot be run, and a
    /// root outside any repository, find nothing either.
    pub fn resolve(reference: &str, root: &Path) -> Option<Self> {
        let directory = root.ancestors().find(|directory| directory.is_dir())?;
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("{reference}^{{commit}}"))
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        // Every file is read from the commit itself, not through the revision's name,
        // so that a ref moving during the run, as a fetch moves `origin/main`, cannot
        // give one run the files of two commits.
        let commit = String::from_utf8(output.stdout).ok()?;
        Some(Self::new(commit.trim()))
    }

    /// Use the shared reader when possible and retry failures through Git show.
    fn read(&self, path: &Path) -> Option<String> {
        let mut batch = self.batch.borrow_mut();
        if matches!(*batch, BatchState::Pending) {
            let directory = path
                .ancestors()
                .skip(1)
                .find(|directory| directory.is_dir())?;
            *batch = Batch::open(directory).map_or(BatchState::Disabled, BatchState::Ready);
        }
        if let BatchState::Ready(reader) = &mut *batch
            && let Ok(relative) = path.strip_prefix(&reader.root)
            && reader.contains(path)
            && let Some(name) = relative
                .components()
                .map(|component| component.as_os_str().to_str())
                .collect::<Option<Vec<_>>>()
        {
            match reader.text(&self.reference, &name.join("/")) {
                Ok(text) => return text,
                // An old git without -Z, or a failed reader, is not evidence
                // that the file was added. Retry with the original git reader.
                Err(_) => *batch = BatchState::Disabled,
            }
        }
        self.read_separately(path)
    }

    /// Ask Git for one file from its nearest surviving parent directory.
    fn read_separately(&self, path: &Path) -> Option<String> {
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
        let known = |reference: &str, root: &Path| Base::resolve(reference, root).is_some();

        assert!(known("HEAD", &repo));
        assert!(known("HEAD", &repo.join("src")), "from a subdirectory");
        assert!(
            known("HEAD", &repo.join("src/gone/deeper")),
            "from a root that is not there, by the nearest directory that is"
        );
        assert!(!known("nosuchrev", &repo));
        assert!(!known("HEAD^{tree}", &repo), "a tree is not a commit");
    }

    /// A ref can move while a run reads through it, as a fetch moves `origin/main`.
    /// Every file is read from the commit the ref named when the run began.
    #[test]
    /// A moving ref must not change the commit used by a live reader.
    fn a_resolved_revision_keeps_reading_the_commit_it_named() {
        let (_keep, repo) = repository(
            &[
                ("src/page.ts", b"export const page = 1;\n"),
                ("src/other.ts", b"export const other = 1;\n"),
            ],
            &[
                ("src/page.ts", b"export const page = 2;\n"),
                ("src/other.ts", b"export const other = 2;\n"),
            ],
        );
        let base = Base::resolve("HEAD", &repo).expect("HEAD names a commit");
        assert_eq!(
            base.text(&repo.join("src/other.ts")).as_deref(),
            Some("export const other = 1;\n")
        );

        git(&repo, &["add", "--all"]);
        git(&repo, &["commit", "--quiet", "--message", "moved"]);

        assert_eq!(
            base.text(&repo.join("src/page.ts")).as_deref(),
            Some("export const page = 1;\n")
        );
    }

    #[test]
    /// Unusual names and contents must not desynchronize successive responses.
    fn successive_reads_keep_their_boundaries_after_missing_and_non_text_files() {
        let large = "export const text = '".to_owned() + &"x".repeat(128 * 1024) + "';\n";
        let before: Vec<(&str, &[u8])> = vec![
            ("src/empty.ts", b""),
            ("src/binary.dat", &[0xff, 0, b'\n']),
            ("src/with space.ts", b"export const space = 1;\n"),
            ("src/unicode-é.ts", b"export const unicode = 1;\n"),
            ("src/large.ts", large.as_bytes()),
            ("src/after.ts", b"export const after = 1;\n"),
        ];
        // These names cannot be created on Windows, but Git's NUL protocol must
        // still handle them on file systems that permit them.
        #[cfg(unix)]
        let before = {
            let mut before = before;
            before.push(("src/line\nbreak.ts", b"export const line = 1;\n"));
            before
        };
        let (_keep, repo) = repository(&before, &[]);
        let base = Base::resolve("HEAD", &repo).unwrap();
        let reads = std::iter::once(("src/not-present.ts", None)).chain(
            before
                .iter()
                .map(|(path, bytes)| (*path, std::str::from_utf8(bytes).ok())),
        );
        for (path, expected) in reads {
            assert_eq!(base.text(&repo.join(path)).as_deref(), expected, "{path:?}");
        }
        // Read an existing file last, after all unusual responses, and from the
        // cache again. The entire directory has been deleted from the checkout.
        assert_eq!(
            base.text(&repo.join("src/after.ts")).as_deref(),
            Some("export const after = 1;\n")
        );
    }

    #[test]
    /// A reader failure must preserve the earlier text through the fallback.
    fn a_broken_batch_reader_retries_a_present_file_instead_of_reporting_it_absent() {
        let (_keep, repo) = repository(
            &[
                ("one.ts", b"export const one = 1;\n"),
                ("two.ts", b"export const two = 2;\n"),
            ],
            &[],
        );
        let base = Base::resolve("HEAD", &repo).unwrap();
        assert!(base.text(&repo.join("one.ts")).is_some());
        let mut batch = base.batch.borrow_mut();
        if let BatchState::Ready(reader) = &mut *batch {
            reader.child.kill().unwrap();
            reader.child.wait().unwrap();
        }
        drop(batch);
        assert_eq!(
            base.text(&repo.join("two.ts")).as_deref(),
            Some("export const two = 2;\n")
        );
    }

    #[test]
    /// Malformed responses must trigger fallback rather than claim absence.
    fn a_truncated_object_response_is_an_error_not_a_missing_file() {
        for response in [
            b"".as_slice(),
            b"oid blob 3",
            b"oid blob nope\0",
            b"oid blob 3\0ab",
            b"oid blob 3\0abc",
            b"oid blob 3\0abc\n",
        ] {
            assert!(read_object(&mut std::io::Cursor::new(response)).is_err());
        }
    }

    #[test]
    /// A shared reader must not capture files owned by a nested repository.
    fn a_nested_repository_is_read_from_its_own_history() {
        let (_keep, repo) = repository(
            &[
                ("root.ts", b"export const root = 1;\n"),
                ("nested/page.ts", b"export const page = 'outer';\n"),
            ],
            &[
                ("root.ts", b"export const root = 2;\n"),
                ("nested/page.ts", b"export const page = 'inner';\n"),
            ],
        );
        let nested = repo.join("nested");
        git(&nested, &["init", "--quiet"]);
        git(&nested, &["add", "--all"]);
        git(&nested, &["commit", "--quiet", "--message", "nested"]);
        let base = Base::new("HEAD");
        assert_eq!(
            base.text(&repo.join("root.ts")).as_deref(),
            Some("export const root = 1;\n")
        );
        assert_eq!(
            base.text(&nested.join("page.ts")).as_deref(),
            Some("export const page = 'inner';\n")
        );
    }
}
