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
/// Step 0 has only `file`, which makes invariants 1 and 3 in section 7.2 vacuous
/// today. The structure is here so that adding a level is the only work step 1 needs
/// to do to get its invariant coverage.
#[derive(Copy, Clone, Debug)]
struct Level {
    name: &'static str,
    args: &'static [&'static str],
}

const LEVELS: &[Level] = &[Level {
    name: "file",
    args: &["--granularity", "file"],
}];

#[derive(Debug, Deserialize)]
struct Expect {
    #[serde(default)]
    anchor: Vec<AnchorExpect>,
}

#[derive(Debug, Deserialize)]
struct AnchorExpect {
    path: String,
    affected: bool,
    /// Expected `--explain` node path. Checked when present; required in spirit for
    /// every must-flag case, because a right verdict by a wrong route is a latent bug.
    #[serde(default)]
    explain: Option<Vec<String>>,
}

struct Outcome {
    affected: bool,
    explain: Vec<String>,
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
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

fn run(case: &Path, anchor: &str, diff_path: &Path, level: Level) -> Outcome {
    let after = case.join("after");
    let mut cmd = Command::new(BINARY);
    cmd.current_dir(&after)
        .arg("--anchor")
        .arg(anchor)
        .arg("--root")
        .arg(&after)
        .arg("--diff")
        .arg(diff_path)
        .arg("--explain")
        .args(level.args);

    let output = cmd.output().expect("running fallout");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let code = output.status.code().unwrap_or(-1);
    assert!(
        code == 0 || code == 1,
        "{} [{}] anchor {}: unexpected exit {}\nstdout: {}\nstderr: {}",
        case.display(),
        level.name,
        anchor,
        code,
        stdout,
        stderr
    );

    let explain = stdout
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("File(") || line.starts_with("Decl("))
        .map(str::to_string)
        .collect();

    Outcome {
        affected: code == 0,
        explain,
    }
}

/// Writes the generated diff next to the fixture's temp copy and returns its path.
fn diff_file(case: &Path, keep: &tempfile::TempDir) -> PathBuf {
    let diff = generate_diff(&case.join("before"), &case.join("after"));
    let path = keep.path().join("generated.diff");
    fs::write(&path, diff).expect("writing generated diff");
    path
}

#[test]
fn fixtures_match_their_expectations() {
    for case in fixture_cases() {
        let expect = read_expect(&case);
        let keep = tempfile::tempdir().unwrap();
        let diff_path = diff_file(&case, &keep);

        for anchor in &expect.anchor {
            for level in LEVELS {
                let outcome = run(&case, &anchor.path, &diff_path, *level);

                assert_eq!(
                    outcome.affected,
                    anchor.affected,
                    "{} [{}] anchor {}: expected affected={}, got {}",
                    case.display(),
                    level.name,
                    anchor.path,
                    anchor.affected,
                    outcome.affected
                );

                if let Some(expected) = &anchor.explain {
                    assert!(
                        anchor.affected,
                        "{} anchor {}: an explain path only makes sense for a must-flag case",
                        case.display(),
                        anchor.path
                    );
                    assert_eq!(
                        &outcome.explain,
                        expected,
                        "{} [{}] anchor {}: wrong path to the change",
                        case.display(),
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
    for case in fixture_cases() {
        let expect = read_expect(&case);
        let keep = tempfile::tempdir().unwrap();
        let diff_path = diff_file(&case, &keep);

        for anchor in expect.anchor.iter().filter(|a| a.affected) {
            for level in LEVELS {
                assert!(
                    run(&case, &anchor.path, &diff_path, *level).affected,
                    "{} [{}] anchor {}: a must-flag case was missed",
                    case.display(),
                    level.name,
                    anchor.path
                );
            }
        }
    }
}

/// Section 7.2, invariant 1: `affected(level n+1)` is a subset of `affected(level n)`.
/// A refinement may only ever remove verdicts.
///
/// Vacuous while `LEVELS` has one entry. It fails loudly the moment a refinement
/// turns a coarse "not affected" into a fine "affected", which would be unsound.
#[test]
fn refinements_only_narrow() {
    for case in fixture_cases() {
        let expect = read_expect(&case);
        let keep = tempfile::tempdir().unwrap();
        let diff_path = diff_file(&case, &keep);

        for anchor in &expect.anchor {
            let verdicts: Vec<(Level, bool)> = LEVELS
                .iter()
                .map(|level| {
                    (
                        *level,
                        run(&case, &anchor.path, &diff_path, *level).affected,
                    )
                })
                .collect();

            for pair in verdicts.windows(2) {
                let (coarse, coarse_affected) = pair[0];
                let (fine, fine_affected) = pair[1];
                assert!(
                    coarse_affected || !fine_affected,
                    "{} anchor {}: {} reports affected but the coarser {} does not",
                    case.display(),
                    anchor.path,
                    fine.name,
                    coarse.name
                );
            }
        }
    }
}

/// A diff and the equivalent `--changed` list must agree at file granularity. This is
/// what keeps the backward-compatible path honest as `--diff` takes over.
#[test]
fn diff_and_changed_paths_agree() {
    for case in fixture_cases() {
        let expect = read_expect(&case);
        let keep = tempfile::tempdir().unwrap();
        let diff_path = diff_file(&case, &keep);

        let before = collect_tree(&case.join("before"));
        let after_tree = collect_tree(&case.join("after"));
        let changed: Vec<String> = after_tree
            .iter()
            .filter(|(path, content)| before.get(*path) != Some(content))
            .map(|(path, _)| path.to_string_lossy().replace('\\', "/"))
            .collect();

        for anchor in &expect.anchor {
            let via_diff = run(&case, &anchor.path, &diff_path, LEVELS[0]).affected;

            let after = case.join("after");
            let mut cmd = Command::new(BINARY);
            cmd.current_dir(&after)
                .arg("--anchor")
                .arg(&anchor.path)
                .arg("--root")
                .arg(&after);
            for path in &changed {
                cmd.arg("--changed").arg(path);
            }
            let via_changed = cmd.output().expect("running fallout").status.code() == Some(0);

            assert_eq!(
                via_diff,
                via_changed,
                "{} anchor {}: --diff and --changed disagree",
                case.display(),
                anchor.path
            );
        }
    }
}
