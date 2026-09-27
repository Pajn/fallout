//! Runs of the analysis made in this process, through the library and through the
//! command line's own entry point, rather than by spawning the binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ahash::AHashMap;
use fallout::base::Earlier;
use fallout::{Granularity, Options, analyse, analyse_with, cli};

/// What a run of the command line said: the exit code, then stdout and stderr.
type Said = (i32, String, String);

/// The command line run in this process.
fn execute(args: &[&str], earlier: Option<Box<dyn Earlier>>) -> Said {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::execute(
        std::iter::once("fallout").chain(args.iter().copied()),
        earlier,
        &mut out,
        &mut err,
    );
    (
        i32::from(code),
        String::from_utf8(out).expect("text on stdout"),
        String::from_utf8(err).expect("text on stderr"),
    )
}

/// The command line run as its own process.
fn spawn(args: &[&str]) -> Said {
    let output = Command::new(env!("CARGO_BIN_EXE_fallout"))
        .args(args)
        .output()
        .expect("running fallout");
    (
        output.status.code().expect("an exit code"),
        String::from_utf8(output.stdout).expect("text on stdout"),
        String::from_utf8(output.stderr).expect("text on stderr"),
    )
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Every file under `root`, by path relative to it.
fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("readable directory") {
            let path = entry.expect("readable entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let relative = path.strip_prefix(root).expect("under root").to_path_buf();
                files.push((relative, fs::read(&path).expect("readable file")));
            }
        }
    }
    files.sort();
    files
}

fn copy(files: &[(PathBuf, Vec<u8>)], to: &Path) {
    for (relative, content) in files {
        let path = to.join(relative);
        fs::create_dir_all(path.parent().expect("a parent")).expect("directory");
        fs::write(path, content).expect("writing file");
    }
}

/// The repository stands alone: no identity, ignore list, hook or signing setting
/// from anywhere else takes part in making its one commit.
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

/// A repository whose one commit holds the fixture's `before` tree, with `after`
/// laid over it: what a `--base HEAD` run reads.
fn repository(fixture: &Path) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let repo = dunce::canonicalize(dir.path()).expect("canonical temp dir");
    copy(&tree(&fixture.join("before")), &repo);
    git(&repo, &["init", "--quiet"]);
    git(&repo, &["add", "--all", "--force"]);
    git(&repo, &["commit", "--quiet", "--message", "before"]);
    clear(&repo);
    copy(&tree(&fixture.join("after")), &repo);
    (dir, repo)
}

/// Empties the working tree without touching the history it was committed to.
fn clear(repo: &Path) {
    for entry in fs::read_dir(repo).expect("readable repository") {
        let path = entry.expect("readable entry").path();
        if path.file_name().is_some_and(|name| name == ".git") {
            continue;
        }
        if path.is_dir() {
            fs::remove_dir_all(&path).expect("removing directory");
        } else {
            fs::remove_file(&path).expect("removing file");
        }
    }
}

/// The fixture's `before` tree as the earlier versions of the files under `root`,
/// leaving out any that are not text.
fn written_down(fixture: &Path, root: &Path) -> AHashMap<PathBuf, String> {
    tree(&fixture.join("before"))
        .into_iter()
        .filter_map(|(relative, content)| {
            Some((root.join(relative), String::from_utf8(content).ok()?))
        })
        .collect()
}

/// The changed files as `git diff --name-only` lists them.
fn changed(fixture: &Path) -> Vec<PathBuf> {
    let before = tree(&fixture.join("before"));
    let after = tree(&fixture.join("after"));
    let mut changed: Vec<PathBuf> = after
        .iter()
        .filter(|file| !before.contains(file))
        .map(|(path, _)| path.clone())
        .chain(
            before
                .iter()
                .filter(|(path, _)| !after.iter().any(|(other, _)| other == path))
                .map(|(path, _)| path.clone()),
        )
        .collect();
    changed.sort();
    changed
}

/// Earlier versions handed over in memory decide exactly what the same versions
/// read from git decide, which is what lets a test run without a repository.
#[test]
fn earlier_versions_in_memory_answer_as_a_base_revision_does() {
    let fixture = fixture("renamed-export");
    let (_keep, repo) = repository(&fixture);
    for anchor in ["src/pages/DatePage.tsx", "src/pages/LegacyPage.tsx"] {
        let options = |base: Option<&str>| Options {
            anchors: vec![PathBuf::from(anchor)],
            changed: changed(&fixture),
            diff: None,
            base: base.map(str::to_string),
            root: repo.clone(),
            only: None,
            granularity: Granularity::Symbol,
            include_types: false,
        };

        let from_git = analyse(&options(Some("HEAD"))).expect("an answer");
        let in_memory: Box<dyn Earlier> = Box::new(written_down(&fixture, &repo));
        // The base named in the options is not read: the versions given are.
        let from_memory =
            analyse_with(&options(Some("no-such-revision")), Some(in_memory)).expect("an answer");
        assert_eq!(from_memory, from_git, "{anchor}");

        let without = analyse_with(&options(Some("HEAD")), None).expect("an answer");
        assert!(
            without.verdict.is_affected(),
            "{anchor}: with no earlier versions every changed file is marked whole"
        );
    }
}

/// The command line run in this process says what the binary says, byte for byte,
/// and ends with the same exit code: verdicts, explanations, JSON, a run with no
/// answer, and the ones clap turns away before any run begins.
#[test]
fn execute_says_what_the_binary_says() {
    let fixture = fixture("renamed-export");
    let root = fixture.join("after");
    let root = root.to_str().expect("a root named in UTF-8");
    let changed = changed(&fixture);
    let changed: Vec<&str> = changed
        .iter()
        .map(|path| path.to_str().expect("a path named in UTF-8"))
        .collect();

    let mut runs: Vec<Vec<&str>> = Vec::new();
    for anchor in ["src/pages/DatePage.tsx", "src/pages/LegacyPage.tsx"] {
        for granularity in ["file", "symbol"] {
            let mut run = vec!["--root", root, "--anchor", anchor];
            run.extend(["--granularity", granularity]);
            for path in &changed {
                run.extend(["--changed", path]);
            }
            runs.push([run.as_slice(), &["--explain", "--unresolved"]].concat());
            runs.push([run.as_slice(), &["--json"]].concat());
        }
    }
    runs.push(vec!["--root", root, "--anchor", "src/pages/Nowhere.tsx"]);
    runs.push(vec!["--root", root, "--anchor", "src/pages/DatePage.tsx"]);
    runs.push(vec!["--root", root, "--diff", "no-such.diff"]);
    runs.push(vec!["--help"]);
    runs.push(vec!["-h"]);
    runs.push(vec!["--version"]);
    runs.push(vec!["--only", "sideways"]);
    runs.push(vec!["--json", "--explain"]);

    for run in runs {
        assert_eq!(execute(&run, None), spawn(&run), "{run:?}");
    }
}
