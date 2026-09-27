//! Fixture harness.
//!
//! Each fixture is a pair of mini projects. The harness generates the unified diff
//! between them, runs the tool against the `after` tree, and checks the verdict and
//! the explained path for every anchor the fixture names.
//!
//! No fixture hand-writes a diff: the input shape is exactly what CI produces from
//! git, so the diff parser is exercised by every case rather than by unit tests alone.
//!
//! The tool runs in this process, through the command line's own entry point, and
//! what the harness reads is what a user would see: the verdict in the exit code
//! and the path in the explained lines. A run that compares against a base revision
//! is handed the `before` tree as the earlier version of each file, which is what
//! the same run reads from git. See `base.rs` for the test that holds the two alike.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use ahash::AHashMap;
use fallout::base::Earlier;
use fallout::{canonical_root, cli};
use serde::Deserialize;

/// One cumulative refinement level from the roadmap.
///
/// The levels are ordered coarsest first, and every invariant in section 7.2 is
/// stated over that order, so adding a level is all a refinement needs to do to get
/// its invariant coverage.
#[derive(Copy, Clone, Debug)]
struct Level {
    name: &'static str,
    args: &'static [&'static str],
    /// Compares each file against an earlier version of itself, as `--base` does, so
    /// the run is handed the `before` tree as those versions.
    ///
    /// The level's arguments leave `--base` out: a revision named here would be read
    /// from whatever repository the harness runs in, which knows nothing of the
    /// fixture.
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
        args: &["--granularity", "symbol"],
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

/// A key the harness does not know is refused rather than ignored, since an
/// expectation nobody checks reads exactly like one that holds.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expect {
    #[serde(default)]
    anchor: Vec<AnchorExpect>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Whether this expectation depends on types being ignored, which is what a
    /// run does unless told otherwise.
    ///
    /// How types are read is not a level: it is a way of reading the code that every
    /// level can be asked for, so it sits beside the chain rather than at the end of
    /// it. Every anchor that sets this is also run with `--include-types` and
    /// required to come out the other way, so that a case which would have passed
    /// regardless cannot pass for the wrong reason.
    #[serde(default)]
    needs_types_ignored: bool,
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

/// One fixture, prepared once: the generated diff, and the earlier version of each
/// file as a `--base` run would read it.
struct Case {
    dir: PathBuf,
    expect: Expect,
    /// Written under the build's own directory for test files, which lasts as long
    /// as the build does, since nothing held in a `static` is ever dropped to clean
    /// up after itself.
    diff: PathBuf,
    /// The `before` tree by the path each file has under the `after` tree, as a run
    /// rooted there names it. A file that is not text is left out, as git's is, and
    /// an added file is simply absent.
    earlier: AHashMap<PathBuf, String>,
}

impl Case {
    fn load(dir: PathBuf) -> Self {
        let diffs = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fixtures");
        fs::create_dir_all(&diffs).expect("a directory for generated diffs");
        let name = dir.file_name().expect("a fixture has a name");
        let diff = diffs.join(name).with_extension("diff");
        // Written aside and moved into place, so that another build's tests writing
        // the same diff at the same moment never leave a run reading half of one.
        let aside = diff.with_extension(format!("diff.{}", std::process::id()));
        fs::write(
            &aside,
            generate_diff(&dir.join("before"), &dir.join("after")),
        )
        .expect("writing generated diff");
        fs::rename(&aside, &diff).expect("moving generated diff into place");

        let root = canonical_root(&dir.join("after"));
        let earlier = collect_tree(&dir.join("before"))
            .into_iter()
            .filter_map(|(relative, content)| {
                Some((root.join(relative), String::from_utf8(content).ok()?))
            })
            .collect();

        Self {
            expect: read_expect(&dir),
            dir,
            diff,
            earlier,
        }
    }

    /// Where every level runs the tool: the `after` tree, in place.
    fn root(&self) -> PathBuf {
        self.dir.join("after")
    }

    /// What a level compares each file against, if it compares at all. Each run has
    /// its own, since an [`Earlier`] is not shared between threads.
    fn earlier(&self, level: Level) -> Option<Box<dyn Earlier>> {
        level
            .versioned
            .then(|| Box::new(self.earlier.clone()) as Box<dyn Earlier>)
    }

    fn name(&self) -> String {
        self.dir.display().to_string()
    }
}

/// Every fixture, loaded once for all the tests in this file.
fn cases() -> &'static [Case] {
    static CASES: OnceLock<Vec<Case>> = OnceLock::new();
    CASES.get_or_init(|| fixture_cases().into_iter().map(Case::load).collect())
}

/// Runs `test` over every fixture, and names each fixture it failed on.
///
/// A panic in one fixture does not stop the others, so a change that breaks several
/// says so in one run, and one raised by the tool rather than by an assertion still
/// says which fixture raised it.
fn each_case(test: impl Fn(&Case)) {
    let failed: Vec<String> = cases()
        .iter()
        .filter_map(|case| {
            let panic = panic::catch_unwind(AssertUnwindSafe(|| test(case))).err()?;
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("a panic with no message");
            Some(format!("{}:\n{}", case.name(), message))
        })
        .collect();
    assert!(
        failed.is_empty(),
        "{} fixture(s) failed:\n\n{}",
        failed.len(),
        failed.join("\n\n")
    );
}

/// What one run wrote and ended with: the exit code, stdout and stderr.
type Said = (u8, String, String);

/// Runs by their whole command line and whether they were handed the tree before,
/// which between them are everything a run's answer depends on.
type Made = HashMap<(Vec<OsString>, bool), Said>;

/// Every run made so far, shared by the tests in this file, so that a run several
/// of them ask for is made once.
fn made() -> MutexGuard<'static, Made> {
    static MADE: OnceLock<Mutex<Made>> = OnceLock::new();
    MADE.get_or_init(Mutex::default)
        .lock()
        // A test that failed while holding the table left it as it was, since
        // nothing panics between reading and writing it.
        .unwrap_or_else(PoisonError::into_inner)
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

/// Runs the command line in this process against the case's `after` tree, at
/// `level`, for `anchor`, with `args` besides. Gives back the exit code and what was
/// written to stdout and stderr. A run some test has already made is not made again.
fn execute(case: &Case, anchor: &str, level: Level, args: Vec<OsString>) -> Said {
    let mut line: Vec<OsString> = vec!["fallout".into(), "--anchor".into(), anchor.into()];
    line.extend(["--root".into(), case.root().into()]);
    line.extend(level.args.iter().map(OsString::from));
    line.extend(args);

    let key = (line, level.versioned);
    if let Some(said) = made().get(&key) {
        return said.clone();
    }
    // Made without holding the table, so the tests running beside this one are not
    // kept waiting. Two of them asking for the same run at once both make it, and
    // get the same answer.
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let code = cli::execute(&key.0, case.earlier(level), &mut stdout, &mut stderr);
    let said = (
        code,
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    );
    made().insert(key, said.clone());
    said
}

fn run(case: &Case, anchor: &str, level: Level) -> Outcome {
    run_with(case, anchor, level, &[])
}

/// The same, with arguments beyond the level's own. Only a test about how types are
/// read has any to add.
fn run_with(case: &Case, anchor: &str, level: Level, extra: &[&str]) -> Outcome {
    let mut args: Vec<OsString> = vec!["--diff".into(), case.diff.clone().into()];
    args.extend(["--explain"].iter().chain(extra).map(OsString::from));
    let (code, stdout, stderr) = execute(case, anchor, level, args);

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
    each_case(|case| {
        for anchor in &case.expect.anchor {
            for level in anchor.levels() {
                let outcome = run(case, &anchor.path, *level);

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
    });
}

/// Section 7.2, invariant 2: every must-flag expectation holds at every level,
/// including the coarsest. This is the test-shaped form of the soundness contract.
#[test]
fn positives_survive_every_level() {
    each_case(|case| {
        for anchor in case.expect.anchor.iter().filter(|a| a.affected) {
            for level in anchor.levels() {
                assert!(
                    run(case, &anchor.path, *level).affected,
                    "{} [{}] anchor {}: a must-flag case was missed",
                    case.name(),
                    level.name,
                    anchor.path
                );
            }
        }
    });
}

/// Section 7.2, invariants 1 and 3: a refinement may only ever remove verdicts from
/// the level it narrows, and the file level is an upper bound on every other.
#[test]
fn refinements_only_narrow() {
    each_case(|case| {
        for anchor in &case.expect.anchor {
            let verdicts: Vec<bool> = LEVELS
                .iter()
                .map(|level| run(case, &anchor.path, *level).affected)
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
    });
}

/// A diff and the equivalent `--changed` list must agree at file granularity.
///
/// `--changed` carries no line information, so it marks whole files at any level. This is
/// what keeps the backward-compatible path honest as `--diff` takes over.
#[test]
fn diff_and_changed_paths_agree() {
    each_case(|case| {
        let before = collect_tree(&case.dir.join("before"));
        let after_tree = collect_tree(&case.dir.join("after"));
        // As `git diff --name-only` lists them: deleted files too.
        let changed: Vec<String> = after_tree
            .iter()
            .filter(|(path, content)| before.get(*path) != Some(content))
            .map(|(path, _)| path)
            .chain(before.keys().filter(|path| !after_tree.contains_key(*path)))
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect();

        for anchor in &case.expect.anchor {
            for level in LEVELS {
                let via_diff = run(case, &anchor.path, *level).affected;

                let args = changed
                    .iter()
                    .flat_map(|path| ["--changed".into(), path.into()])
                    .collect();
                let via_changed = execute(case, &anchor.path, *level, args).0 == 0;

                assert!(
                    via_changed || !via_diff,
                    "{} [{}] anchor {}: --diff flags but the coarser --changed does not",
                    case.name(),
                    level.name,
                    anchor.path
                );
            }
        }
    });
}

/// Section 7.2, invariant 1 again, for the reading rather than the level: ignoring
/// types may only ever remove verdicts. This holds for every fixture, not only the
/// ones written about types, so every case in the suite tests how types are read.
#[test]
fn the_default_narrows_from_a_read_as_written() {
    each_case(|case| {
        for anchor in &case.expect.anchor {
            for level in LEVELS {
                let as_written =
                    run_with(case, &anchor.path, *level, &["--include-types"]).affected;
                let default = run(case, &anchor.path, *level).affected;
                assert!(
                    as_written || !default,
                    "{} [{}] anchor {}: the default reports affected but --include-types does not",
                    case.name(),
                    level.name,
                    anchor.path
                );
            }
        }
    });
}

/// A must-skip written about types has to be the erasure's doing.
///
/// Without this, a fixture whose change reaches nobody either way would sit in the
/// suite looking like evidence and proving nothing. Asserting the flip at the
/// coarsest level the expectation is held at is the strongest form available: it is
/// the level with the least to go on, so the others follow.
///
/// A must-flag needs no such check. That one is the soundness contract, and
/// [`positives_survive_every_level`] already holds it at every level.
#[test]
fn a_types_case_needs_types_ignored() {
    each_case(|case| {
        for anchor in case
            .expect
            .anchor
            .iter()
            .filter(|a| a.needs_types_ignored && !a.affected)
        {
            let level = anchor.levels()[0];
            assert!(
                run_with(case, &anchor.path, level, &["--include-types"]).affected,
                "{} [{}] anchor {}: reads the same whether or not types are ignored",
                case.name(),
                level.name,
                anchor.path
            );
        }
    });
}

/// Everything the tool says about a fixture: for each anchor it names, at every
/// level and read both ways, the exit code, the explained verdict and the JSON.
///
/// The expectations say what must hold; this says what does, down to the byte, so
/// that a change to what a user sees cannot pass unnoticed because no expectation
/// happened to look at it. A change meant to alter it is blessed: see
/// [`answers_match_their_snapshots`].
fn answers(case: &Case) -> String {
    let mut text = String::from(
        "# What fallout says about this fixture, written by tests/fixtures.rs. For each\n\
         # anchor, at every level and read both ways: the exit code and output of a run\n\
         # with --explain, then of one with --json.\n",
    );
    for anchor in &case.expect.anchor {
        for level in LEVELS {
            for reading in [None, Some("--include-types")] {
                text.push_str(&format!("\n== {} [{}]", anchor.path, level.name));
                if let Some(flag) = reading {
                    text.push_str(&format!(" {flag}"));
                }
                text.push('\n');
                for shape in ["--explain", "--json"] {
                    let mut args: Vec<OsString> = vec!["--diff".into(), case.diff.clone().into()];
                    args.extend([shape].into_iter().chain(reading).map(OsString::from));
                    let (code, stdout, stderr) = execute(case, &anchor.path, *level, args);
                    text.push_str(&format!("-- {shape}: exit {code}\n{stdout}"));
                    if !stderr.is_empty() {
                        text.push_str(&format!("-- stderr\n{stderr}"));
                    }
                }
            }
        }
    }
    text
}

/// Each fixture's answers are what its `answers.txt` records.
///
/// `FALLOUT_BLESS=1 cargo test --test fixtures` writes what the tool says now in
/// place of what was recorded, for a change that means to alter it. The diff of
/// the files is then the change a user would see.
#[test]
fn answers_match_their_snapshots() {
    let bless = std::env::var_os("FALLOUT_BLESS").is_some_and(|value| value == "1");
    // Blessing rewrites what is recorded, which a CI run must never do quietly.
    assert!(
        !(bless && std::env::var_os("CI").is_some()),
        "FALLOUT_BLESS is set in CI, where the recorded answers are checked, not written"
    );
    each_case(|case| {
        let path = case.dir.join("answers.txt");
        let now = answers(case);
        if bless {
            fs::write(&path, &now).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
            return;
        }
        // A checkout that writes line endings its own way changes nothing recorded.
        let recorded = match fs::read_to_string(&path) {
            Ok(text) => text.replace("\r\n", "\n"),
            Err(error) => panic!(
                "{} cannot be read ({error}). A new fixture gets one with \
                 `FALLOUT_BLESS=1 cargo test --test fixtures`.",
                path.display()
            ),
        };
        if recorded != now {
            let difference = similar::TextDiff::from_lines(&recorded, &now)
                .unified_diff()
                .context_radius(2)
                .header("recorded", "now")
                .to_string();
            panic!(
                "{} does not record what the tool says now. If the change is meant, \
                 bless it with `FALLOUT_BLESS=1 cargo test --test fixtures` and review \
                 the diff.\n{difference}",
                path.display()
            );
        }
    });
}
