//! The binary itself, spawned as a user runs it.
//!
//! Everything about an answer is tested in this process, through `cli::execute`: see
//! `analysis.rs` and `fixtures.rs`. What is left here is what only the process can
//! show: the exit code it ends with, what it prints where, the directory it runs
//! from, and a `--base` revision read from a real repository. CI also runs this file
//! against the release build.

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use common::{changed, fixture, repository, setup_test_project, setup_unresolved_project};
use fallout::cli;
use tempfile::TempDir;

const BINARY: &str = env!("CARGO_BIN_EXE_fallout");

fn run_is_affected(root: &Path, anchors: &[&str], changed: &[&str]) -> (i32, String, String) {
    run_is_affected_with(root, anchors, changed, &[])
}

fn run_is_affected_with(
    root: &Path,
    anchors: &[&str],
    changed: &[&str],
    extra_args: &[&str],
) -> (i32, String, String) {
    let mut cmd = Command::new(BINARY);
    cmd.current_dir(root);

    for anchor in anchors {
        cmd.arg("--anchor").arg(anchor);
    }

    for change in changed {
        cmd.arg("--changed").arg(change);
    }

    cmd.arg("--root").arg(root);

    for arg in extra_args {
        cmd.arg(arg);
    }

    let output = cmd.output().expect("Failed to execute is_affected");

    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// What a run of the command line said: the exit code, then stdout and stderr.
type Said = (i32, String, String);

/// The command line run in this process.
fn execute(args: &[&str]) -> Said {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    // The program name clap prints comes from the first argument, so this passes
    // the one the binary is started with: its file name ends in `.exe` on Windows.
    let code = cli::execute(
        std::iter::once(env!("CARGO_BIN_EXE_fallout")).chain(args.iter().copied()),
        None,
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
    let output = Command::new(BINARY)
        .args(args)
        .output()
        .expect("running fallout");
    (
        output.status.code().expect("an exit code"),
        String::from_utf8(output.stdout).expect("text on stdout"),
        String::from_utf8(output.stderr).expect("text on stderr"),
    )
}

/// The command line run in this process says what the binary says, byte for byte,
/// and ends with the same exit code: verdicts, explanations, JSON, a run with no
/// answer, and the ones clap turns away before any run begins. This is what lets
/// every other test of an answer run in this process.
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
    runs.push(vec![
        "--root",
        root,
        "--anchor",
        "src/pages/DatePage.tsx",
        "--base",
        "no-such-revision",
    ]);
    runs.push(vec!["--help"]);
    runs.push(vec!["-h"]);
    runs.push(vec!["--version"]);
    runs.push(vec!["--only", "sideways"]);
    runs.push(vec!["--json", "--explain"]);

    for run in runs {
        assert_eq!(execute(&run), spawn(&run), "{run:?}");
    }
}

/// With no `--root`, the root is the directory the command runs in: anchors and
/// changed paths are read from there, and paths are printed relative to it.
#[test]
fn the_root_defaults_to_the_directory_it_runs_in() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let output = Command::new(BINARY)
        .current_dir(&root)
        .args(["--anchor", "src/pages/CheckoutPage.tsx"])
        .args(["--changed", "src/components/Button.tsx"])
        .arg("--explain")
        .output()
        .expect("running fallout");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Impact detected on target anchor via: \"src/components/Button.tsx\"\n\
         Path (downstream, file granularity):\n  \
         File(src/pages/CheckoutPage.tsx)\n  \
         File(src/components/Card.tsx)\n  \
         File(src/components/Button.tsx)\n"
    );

    let output = Command::new(BINARY)
        .current_dir(&root)
        .args(["--anchor", "src/pages/SettingsPage.tsx"])
        .args(["--changed", "src/utils/helpers.ts"])
        .output()
        .expect("running fallout");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "No reachability impact detected\n"
    );
}

/// A `--base` run over a real repository, the way CI makes one.
fn with_base(fixture_name: &str, anchor: &str) -> (Option<i32>, String) {
    let fixture = fixture(fixture_name);
    let (_keep, repo) = repository(&fixture);
    let mut command = Command::new(BINARY);
    command.current_dir(&repo).args(["--anchor", anchor]).args([
        "--granularity",
        "symbol",
        "--base",
        "HEAD",
        "--explain",
    ]);
    for path in changed(&fixture) {
        command.arg("--changed").arg(path);
    }
    let output = command.output().expect("running fallout");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// A file whose syntax is the same either side marks nothing once it is compared
/// with the revision git holds, where without one the whole file counts as changed.
#[test]
fn a_base_revision_finds_a_change_of_comments_and_layout_unchanged() {
    let (code, stdout) = with_base("trivia-only", "src/pages/PricePage.tsx");
    assert_eq!(code, Some(1), "{stdout}");
    assert_eq!(stdout, "No reachability impact detected\n");
}

/// An export the revision had and the change removed reaches the page still asking
/// for it, and only that page.
#[test]
fn a_base_revision_finds_the_exports_a_change_lost() {
    let (code, stdout) = with_base("renamed-export", "src/pages/LegacyPage.tsx");
    assert_eq!(code, Some(0), "{stdout}");
    assert_eq!(
        stdout,
        "Impact detected on target anchor via: \"src/utils/helpers.ts\"\n\
         Path (downstream, symbol granularity):\n  \
         File(src/pages/LegacyPage.tsx)\n  \
         Decl(src/pages/LegacyPage.tsx, LegacyPage)\n  \
         Export(src/utils/helpers.ts, formatPrice)\n"
    );

    let (code, stdout) = with_base("renamed-export", "src/pages/DatePage.tsx");
    assert_eq!(code, Some(1), "{stdout}");
}

/// `--changed` takes paths the way `git diff --name-only` writes them, relative to
/// the root, wherever the command runs from.
#[test]
fn changed_paths_are_relative_to_the_root() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().join("project");
    fs::create_dir_all(&root).unwrap();
    setup_test_project(&root);

    for granularity in ["file", "symbol"] {
        let output = Command::new(BINARY)
            .current_dir(temp_dir.path())
            .args(["--anchor", "src/pages/SettingsPage.tsx"])
            .args(["--changed", "src/components/Button.tsx"])
            .arg("--root")
            .arg(&root)
            .args(["--granularity", granularity])
            .output()
            .expect("Failed to execute is_affected");

        assert_eq!(
            output.status.code(),
            Some(0),
            "[{granularity}] a path relative to the root names the root's file: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn no_anchor_error() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let mut cmd = Command::new(BINARY);
    cmd.current_dir(&root);
    cmd.arg("--changed").arg("src/components/Button.tsx");
    cmd.arg("--root").arg(&root);

    let output = cmd.output().expect("Failed to execute is_affected");

    assert_eq!(
        output.status.code(),
        Some(2),
        "no anchor is no answer, which is exit code 2"
    );
}

#[test]
fn invalid_anchor_error() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let (code, stdout, stderr) = run_is_affected(
        &root,
        &["src/pages/NonExistentPage.tsx"],
        &["src/components/Button.tsx"],
    );

    assert_eq!(
        code, 2,
        "a missing anchor is no answer, which is exit code 2, got {}. stdout: {} stderr: {}",
        code, stdout, stderr
    );
    assert!(
        stderr.contains("Anchor(s) not found"),
        "Expected error message about missing anchor, got stderr: {}",
        stderr
    );
}

#[test]
fn only_short_flag_matches_long_flag() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let (code, stdout, _stderr) = run_is_affected_with(
        &root,
        &["src/pages/CheckoutPage.tsx"],
        &["src/components/Button.tsx"],
        &["-o", "downstream"],
    );

    assert_eq!(
        code, 0,
        "Expected exit code 0 (short flag), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("src/components/Button.tsx"));
}

#[test]
fn only_rejects_unknown_direction() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let (code, stdout, stderr) = run_is_affected_with(
        &root,
        &["src/pages/CheckoutPage.tsx"],
        &["src/components/Button.tsx"],
        &["--only", "sideways"],
    );

    assert_ne!(
        code, 0,
        "Expected non-zero exit code for an unknown direction, got {}. stdout: {}",
        code, stdout
    );
    assert!(
        stderr.contains("sideways"),
        "Expected the error to name the bad value, got stderr: {}",
        stderr
    );
}

#[test]
fn exit_code_tells_not_affected_from_no_answer() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_test_project(&root);

    let affected = run_is_affected(
        &root,
        &["src/pages/CheckoutPage.tsx"],
        &["src/components/Button.tsx"],
    );
    assert_eq!(affected.0, 0, "affected: {affected:?}");

    let unaffected = run_is_affected(
        &root,
        &["src/pages/SettingsPage.tsx"],
        &["src/utils/helpers.ts"],
    );
    assert_eq!(unaffected.0, 1, "not affected: {unaffected:?}");

    let missing = run_is_affected(&root, &["src/pages/Nope.tsx"], &[]);
    assert_eq!(missing.0, 2, "no answer: {missing:?}");

    let unreadable = run_is_affected_with(
        &root,
        &["src/pages/CheckoutPage.tsx"],
        &["src/components/Button.tsx"],
        &["--diff", "no-such.diff"],
    );
    assert_eq!(unreadable.0, 2, "no answer: {unreadable:?}");
}

#[test]
fn json_does_not_take_flags_it_already_answers() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_test_project(&root);
    for flag in ["--explain", "--unresolved"] {
        let (code, _, stderr) = run_is_affected_with(
            &root,
            &["src/pages/CheckoutPage.tsx"],
            &["src/components/Button.tsx"],
            &["--json", flag],
        );
        assert_eq!(code, 2, "{flag}: {stderr}");
    }
}

/// The flag reports and does not judge: same verdict, same exit code.
#[test]
fn unresolved_does_not_change_the_verdict() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_unresolved_project(&root);

    let plain = run_is_affected_with(
        &root,
        &["src/pages/LostPage.tsx"],
        &["src/utils/lost.ts"],
        &["--granularity", "symbol"],
    );
    let listed = run_is_affected_with(
        &root,
        &["src/pages/LostPage.tsx"],
        &["src/utils/lost.ts"],
        &["--granularity", "symbol", "--unresolved"],
    );

    assert_eq!(plain.0, listed.0, "same exit code");
    assert!(
        listed.1.starts_with(&plain.1),
        "the verdict is untouched and the report follows it:\n{}\n---\n{}",
        plain.1,
        listed.1
    );
}

/// A config the run needs and cannot read replaces the verdict.
///
/// Silence is the failure mode this guards against. A setting that quietly does
/// nothing shows up later as a verdict nobody can explain, and the whole point of
/// declaring it was to be believed.
#[test]
fn unreadable_config_is_reported_rather_than_ignored() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_test_project(&root);

    fs::write(
        root.join("src/components/fallout.toml"),
        "inline-requires = 3\n",
    )
    .unwrap();

    let (code, stdout, stderr) = run_is_affected_with(
        &root,
        &["src/components/Card.tsx"],
        &["src/components/Button.tsx"],
        &["--granularity", "symbol"],
    );

    let said = format!("{stdout}{stderr}");
    assert_eq!(
        code, 2,
        "a config that cannot be read is not a verdict: {said}"
    );
    assert!(
        said.contains("inline-requires"),
        "the message names the setting: {said}"
    );
}

/// A revision git cannot find is no answer. Read as one that holds none of the
/// files, it would have every changed config taken as one the change added, and the
/// imports that config governs not taken as moved: here the page would be found
/// not affected.
#[test]
fn a_base_revision_git_cannot_find_is_no_answer() {
    let fixture = fixture("tsconfig-extends-missing-file");
    let (_keep, repo) = repository(&fixture);
    let root = repo.to_str().expect("a root named in UTF-8");
    let run = [
        "--root",
        root,
        "--anchor",
        "src/page.ts",
        "--changed",
        "tsconfig.json",
        "--base",
        "nosuchrev",
    ];
    for extra in [&[][..], &["--json"]] {
        let (code, stdout, stderr) = spawn(&[&run[..], extra].concat());
        assert_eq!(code, 2, "{extra:?}: {stdout}{stderr}");
        assert_eq!(stdout, "", "{extra:?}");
        assert!(
            stderr.starts_with("Error: ") && stderr.contains("nosuchrev"),
            "{extra:?}: the message names the revision: {stderr}"
        );
    }
}

/// Outside a repository no revision can be read, so a `--base` run there has no
/// answer either.
#[test]
fn a_base_revision_outside_a_repository_is_no_answer() {
    let temp = TempDir::new().unwrap();
    let root = dunce::canonicalize(temp.path()).expect("canonical temp dir");
    setup_test_project(&root);

    // Git looks for a repository no higher than the temporary directory, so none
    // that encloses it takes part.
    let output = Command::new(BINARY)
        .env("GIT_CEILING_DIRECTORIES", root.parent().expect("a parent"))
        .env_remove("GIT_DIR")
        .arg("--root")
        .arg(&root)
        .args(["--anchor", "src/pages/CheckoutPage.tsx"])
        .args(["--changed", "src/components/Button.tsx"])
        .args(["--base", "HEAD"])
        .output()
        .expect("running fallout");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert!(
        stderr.contains("HEAD"),
        "the message names the revision: {stderr}"
    );
}
