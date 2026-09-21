//! Fixture harness.
//!
//! Each fixture is a pair of mini projects. The harness generates the unified diff
//! between them, runs the tool against the `after` tree, and checks the verdict and
//! the explained path for every anchor the fixture names.
//!
//! No fixture hand-writes a diff: the input shape is exactly what CI produces from
//! git, so the diff parser is exercised by every case rather than by unit tests alone.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

const BINARY: &str = env!("CARGO_BIN_EXE_fallout");

/// One cumulative refinement level from the roadmap.
///
/// The levels are ordered coarsest first, and every invariant in section 7.2 is
/// stated over that order, so adding a level is all a refinement needs to do to get
/// its invariant coverage.
#[derive(Copy, Clone, Debug)]
struct Level {
    name: &'static str,
    args: &'static [&'static str],
    /// Compares each file against an earlier version of itself, so it needs that
    /// version to exist in git.
    versioned: bool,
    /// The level this one may only ever narrow from.
    ///
    /// The granularity levels form a chain, each describing the code more finely
    /// than the last. A base revision is not a link in that chain: it describes the
    /// *change* rather than the code, and it sees things a line range cannot — a
    /// removed declaration, a swapped pair of statements — so it is bounded by the
    /// file level and by nothing in between.
    narrows_from: Option<&'static str>,
}

const LEVELS: &[Level] = &[
    Level {
        name: "file",
        args: &["--granularity", "file"],
        versioned: false,
        narrows_from: None,
    },
    Level {
        name: "symbol",
        args: &["--granularity", "symbol"],
        versioned: false,
        narrows_from: Some("file"),
    },
    Level {
        name: "base",
        args: &["--granularity", "symbol", "--base", "HEAD"],
        versioned: true,
        narrows_from: Some("file"),
    },
];

fn level(name: &str) -> Level {
    *LEVELS
        .iter()
        .find(|level| level.name == name)
        .unwrap_or_else(|| panic!("no such level: {name}"))
}

fn position(name: &str) -> usize {
    LEVELS
        .iter()
        .position(|level| level.name == name)
        .unwrap_or_else(|| panic!("no such level: {name}"))
}

#[derive(Debug, Deserialize)]
struct Expect {
    #[serde(default)]
    anchor: Vec<AnchorExpect>,
}

#[derive(Debug, Deserialize)]
struct AnchorExpect {
    path: String,
    /// `true` is a must-flag: the soundness contract, held at every level from
    /// `from` onwards. `false` is a must-skip, held the same way.
    affected: bool,
    /// The coarsest level this expectation is held at.
    ///
    /// A must-flag defaults to the coarsest level there is, because never missing a
    /// change is the contract every level signs. A must-skip defaults to the
    /// coarsest level that reasons about code rather than about text, because a
    /// whole-file verdict is allowed to over-report and nothing else is. A fixture
    /// names a level here only when what it turns on is the description of the
    /// change rather than the analysis of the code.
    #[serde(default)]
    from: Option<String>,
    /// Run this anchor with `--ignore-types`.
    ///
    /// The flag is not a level: it is a way of reading the code that every level can
    /// be asked for, so it sits beside the chain rather than at the end of it. Every
    /// anchor that sets it is also checked to come out the other way without it, so
    /// that a case which would have passed regardless cannot pass for the wrong
    /// reason.
    #[serde(default)]
    ignore_types: bool,
    /// Expected `--explain` node path, keyed by level name. A right verdict reached
    /// by a wrong route is a latent bug, so this is checked wherever it is given.
    #[serde(default)]
    explain: BTreeMap<String, Vec<String>>,
}

impl AnchorExpect {
    /// The levels this anchor's expectation is held at, coarsest first.
    fn levels(&self) -> &'static [Level] {
        let default = if self.affected { "file" } else { "symbol" };
        &LEVELS[position(self.from.as_deref().unwrap_or(default))..]
    }
}

struct Outcome {
    affected: bool,
    explain: Vec<String>,
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// One fixture, prepared once: the generated diff, and the `after` tree laid over a
/// repository whose `HEAD` holds `before`, which is what a `--base` run reads.
struct Case {
    dir: PathBuf,
    expect: Expect,
    diff: PathBuf,
    versioned: PathBuf,
    /// Deletes both of the above when the case goes out of scope.
    _keep: tempfile::TempDir,
}

impl Case {
    fn load(dir: PathBuf) -> Self {
        let keep = tempfile::tempdir().expect("temp dir");
        let diff = keep.path().join("generated.diff");
        fs::write(
            &diff,
            generate_diff(&dir.join("before"), &dir.join("after")),
        )
        .expect("writing generated diff");

        let versioned = keep.path().join("repo");
        build_repo(&dir, &versioned);

        Self {
            expect: read_expect(&dir),
            dir,
            diff,
            versioned,
            _keep: keep,
        }
    }

    /// Where a level runs the tool.
    fn root(&self, level: Level) -> PathBuf {
        if level.versioned {
            self.versioned.clone()
        } else {
            self.dir.join("after")
        }
    }

    fn name(&self) -> String {
        self.dir.display().to_string()
    }
}

fn cases() -> Vec<Case> {
    fixture_cases().into_iter().map(Case::load).collect()
}

/// A repository holding the fixture's `before` tree as its only commit, with the
/// `after` tree checked out over the top: the shape a real `--base` run sees.
fn build_repo(case: &Path, repo: &Path) {
    fs::create_dir_all(repo).expect("creating fixture repository");
    copy_tree(&case.join("before"), repo);
    git(repo, &["init", "--quiet"]);
    git(repo, &["add", "--all", "--force"]);
    git(
        repo,
        &["commit", "--quiet", "--allow-empty", "--message", "before"],
    );

    clear_tree(repo);
    copy_tree(&case.join("after"), repo);
}

/// The fixture repository stands alone: no identity, ignore list, hook or signing
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
    assert!(
        status.success(),
        "git {:?} failed in {}",
        args,
        repo.display()
    );
}

fn copy_tree(source: &Path, destination: &Path) {
    for (relative, content) in collect_tree(source) {
        let path = destination.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("creating fixture directory");
        }
        fs::write(&path, content).expect("writing fixture file");
    }
}

/// Empties the working tree without touching the history it was committed to.
fn clear_tree(repo: &Path) {
    for entry in fs::read_dir(repo).expect("readable repository") {
        let path = entry.expect("readable entry").path();
        if path.file_name().is_some_and(|name| name == ".git") {
            continue;
        }
        if path.is_dir() {
            fs::remove_dir_all(&path).expect("removing fixture directory");
        } else {
            fs::remove_file(&path).expect("removing fixture file");
        }
    }
}

fn fixture_cases() -> Vec<PathBuf> {
    let mut cases: Vec<PathBuf> = fs::read_dir(fixtures_dir())
        .expect("tests/fixtures should exist")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "no fixtures found");
    cases
}

fn read_expect(case: &Path) -> Expect {
    let path = case.join("expect.toml");
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {}", path.display(), e));
    toml::from_str(&text).unwrap_or_else(|e| panic!("parsing {}: {}", path.display(), e))
}

/// Every file under `root`, keyed by its path relative to `root`.
fn collect_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    if root.is_dir() {
        collect_into(root, root, &mut files);
    }
    files
}

fn collect_into(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
    for entry in fs::read_dir(dir).expect("readable fixture directory") {
        let path = entry.expect("readable entry").path();
        if path.is_dir() {
            collect_into(root, &path, files);
        } else {
            let relative = path
                .strip_prefix(root)
                .expect("path under root")
                .to_path_buf();
            files.insert(relative, fs::read(&path).expect("readable fixture file"));
        }
    }
}

/// Builds a git-shaped unified diff from the two trees.
fn generate_diff(before: &Path, after: &Path) -> String {
    let old_tree = collect_tree(before);
    let new_tree = collect_tree(after);

    let mut paths: Vec<&PathBuf> = old_tree.keys().chain(new_tree.keys()).collect();
    paths.sort();
    paths.dedup();

    let mut diff = String::new();
    for path in paths {
        let display = path.to_string_lossy().replace('\\', "/");
        let old = old_tree.get(path);
        let new = new_tree.get(path);

        match (old, new) {
            (Some(old), Some(new)) if old == new => continue,
            _ => {}
        }

        diff.push_str(&format!("diff --git a/{} b/{}\n", display, display));

        // A file either side cannot decode as text has no line structure to diff.
        let old_text = old.map(|b| String::from_utf8(b.clone()));
        let new_text = new.map(|b| String::from_utf8(b.clone()));
        if matches!(old_text, Some(Err(_))) || matches!(new_text, Some(Err(_))) {
            diff.push_str(&format!(
                "Binary files a/{} and b/{} differ\n",
                display, display
            ));
            continue;
        }

        let old_text = old_text
            .map(|t| t.expect("checked above"))
            .unwrap_or_default();
        let new_text = new_text
            .map(|t| t.expect("checked above"))
            .unwrap_or_default();

        let (old_header, new_header) = match (old, new) {
            (None, _) => ("/dev/null".to_string(), format!("b/{}", display)),
            (_, None) => (format!("a/{}", display), "/dev/null".to_string()),
            _ => (format!("a/{}", display), format!("b/{}", display)),
        };

        let text_diff = similar::TextDiff::from_lines(&old_text, &new_text);
        diff.push_str(
            &text_diff
                .unified_diff()
                .context_radius(3)
                .header(&old_header, &new_header)
                .to_string(),
        );
    }
    diff
}

fn run(case: &Case, anchor: &str, level: Level, ignore_types: bool) -> Outcome {
    let root = case.root(level);
    let mut cmd = Command::new(BINARY);
    cmd.current_dir(&root)
        .arg("--anchor")
        .arg(anchor)
        .arg("--root")
        .arg(&root)
        .arg("--diff")
        .arg(&case.diff)
        .arg("--explain")
        .args(level.args);
    if ignore_types {
        cmd.arg("--ignore-types");
    }

    let output = cmd.output().expect("running fallout");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let code = output.status.code().unwrap_or(-1);
    assert!(
        code == 0 || code == 1,
        "{} [{}] anchor {}: unexpected exit {}\nstdout: {}\nstderr: {}",
        case.name(),
        level.name,
        anchor,
        code,
        stdout,
        stderr
    );

    // Everything indented under the `Path (...)` header is a node, whatever kind it
    // is. Matching on node kinds here would silently drop the kinds added later.
    let explain = stdout
        .lines()
        .skip_while(|line| !line.starts_with("Path ("))
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .map(|line| line.trim().to_string())
        .collect();

    Outcome {
        affected: code == 0,
        explain,
    }
}

#[test]
fn fixtures_match_their_expectations() {
    for case in cases() {
        for anchor in &case.expect.anchor {
            for level in anchor.levels() {
                let outcome = run(&case, &anchor.path, *level, anchor.ignore_types);

                assert_eq!(
                    outcome.affected,
                    anchor.affected,
                    "{} [{}] anchor {}: expected affected = {}",
                    case.name(),
                    level.name,
                    anchor.path,
                    anchor.affected
                );

                if let Some(expected) = anchor.explain.get(level.name) {
                    assert!(
                        anchor.affected,
                        "{} anchor {}: an explain path only makes sense for a must-flag case",
                        case.name(),
                        anchor.path
                    );
                    assert_eq!(
                        &outcome.explain,
                        expected,
                        "{} [{}] anchor {}: wrong path to the change",
                        case.name(),
                        level.name,
                        anchor.path
                    );
                }
            }
        }
    }
}

/// Section 7.2, invariant 2: every must-flag expectation holds at every level,
/// including the coarsest. This is the test-shaped form of the soundness contract.
#[test]
fn positives_survive_every_level() {
    for case in cases() {
        for anchor in case.expect.anchor.iter().filter(|a| a.affected) {
            for level in anchor.levels() {
                assert!(
                    run(&case, &anchor.path, *level, anchor.ignore_types).affected,
                    "{} [{}] anchor {}: a must-flag case was missed",
                    case.name(),
                    level.name,
                    anchor.path
                );
            }
        }
    }
}

/// Section 7.2, invariants 1 and 3: a refinement may only ever remove verdicts from
/// the level it narrows, and the file level is an upper bound on every other.
#[test]
fn refinements_only_narrow() {
    for case in cases() {
        for anchor in &case.expect.anchor {
            let verdicts: Vec<bool> = LEVELS
                .iter()
                .map(|level| run(&case, &anchor.path, *level, anchor.ignore_types).affected)
                .collect();

            for (index, fine) in LEVELS.iter().enumerate() {
                let Some(coarser) = fine.narrows_from else {
                    continue;
                };
                assert!(
                    verdicts[position(coarser)] || !verdicts[index],
                    "{} anchor {}: {} reports affected but the coarser {} does not",
                    case.name(),
                    anchor.path,
                    fine.name,
                    level(coarser).name
                );
            }
        }
    }
}

/// A diff and the equivalent `--changed` list must agree at file granularity.
///
/// `--changed` carries no line information, so it marks whole files at any level. This is
/// what keeps the backward-compatible path honest as `--diff` takes over.
#[test]
fn diff_and_changed_paths_agree() {
    for case in cases() {
        let before = collect_tree(&case.dir.join("before"));
        let after_tree = collect_tree(&case.dir.join("after"));
        let changed: Vec<String> = after_tree
            .iter()
            .filter(|(path, content)| before.get(*path) != Some(content))
            .map(|(path, _)| path.to_string_lossy().replace('\\', "/"))
            .collect();

        for anchor in &case.expect.anchor {
            for level in LEVELS {
                let via_diff = run(&case, &anchor.path, *level, anchor.ignore_types).affected;

                let root = case.root(*level);
                let mut cmd = Command::new(BINARY);
                cmd.current_dir(&root)
                    .arg("--anchor")
                    .arg(&anchor.path)
                    .arg("--root")
                    .arg(&root)
                    .args(level.args);
                if anchor.ignore_types {
                    cmd.arg("--ignore-types");
                }
                for path in &changed {
                    cmd.arg("--changed").arg(path);
                }
                let via_changed = cmd.output().expect("running fallout").status.code() == Some(0);

                assert!(
                    via_changed || !via_diff,
                    "{} [{}] anchor {}: --diff flags but the coarser --changed does not",
                    case.name(),
                    level.name,
                    anchor.path
                );
            }
        }
    }
}

/// Section 7.2, invariant 1 again, for the reading rather than the level: ignoring
/// types may only ever remove verdicts. This holds for every fixture, not only the
/// ones written about types, so every case in the suite is a test of the flag.
#[test]
fn ignoring_types_only_narrows() {
    for case in cases() {
        for anchor in &case.expect.anchor {
            for level in LEVELS {
                let plain = run(&case, &anchor.path, *level, false).affected;
                let ignored = run(&case, &anchor.path, *level, true).affected;
                assert!(
                    plain || !ignored,
                    "{} [{}] anchor {}: --ignore-types reports affected but a plain read does not",
                    case.name(),
                    level.name,
                    anchor.path
                );
            }
        }
    }
}

/// A must-skip written about `--ignore-types` has to be the flag's doing.
///
/// Without this, a fixture whose change reaches nobody either way would sit in the
/// suite looking like evidence and proving nothing. Asserting the flip at the
/// coarsest level the expectation is held at is the strongest form available: it is
/// the level with the least to go on, so the others follow.
///
/// A must-flag needs no such check. That one is the soundness contract, and
/// [`positives_survive_every_level`] already holds it at every level with the flag
/// turned on.
#[test]
fn a_types_case_needs_the_flag() {
    for case in cases() {
        for anchor in case
            .expect
            .anchor
            .iter()
            .filter(|a| a.ignore_types && !a.affected)
        {
            let level = anchor.levels()[0];
            assert!(
                run(&case, &anchor.path, level, false).affected,
                "{} [{}] anchor {}: reads the same with and without --ignore-types",
                case.name(),
                level.name,
                anchor.path
            );
        }
    }
}
