//! Runs of the analysis made in this process, through the library and through the
//! command line's own entry point, rather than by spawning the binary.
//!
//! `cli.rs` holds that the command line run here says what the binary says, so
//! each of these reads exactly what a user would.

mod common;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use ahash::AHashMap;
#[cfg(unix)]
use common::Unreadable;
use common::{
    changed, fixture, repository, setup_bundler_package, setup_test_project,
    setup_tsconfig_project, setup_unresolved_project, tree,
};
use fallout::base::Earlier;
use fallout::{Granularity, Options, analyse, analyse_with, cli};
use tempfile::TempDir;

fn run_is_affected(root: &Path, anchors: &[&str], changed: &[&str]) -> (i32, String, String) {
    run_is_affected_with(root, anchors, changed, &[])
}

/// The command line, run in this process with the arguments a user would give it.
fn run_is_affected_with(
    root: &Path,
    anchors: &[&str],
    changed: &[&str],
    extra_args: &[&str],
) -> (i32, String, String) {
    let mut args: Vec<OsString> = vec!["fallout".into()];
    for anchor in anchors {
        args.extend(["--anchor".into(), anchor.into()]);
    }
    for change in changed {
        args.extend(["--changed".into(), change.into()]);
    }
    args.extend(["--root".into(), root.into()]);
    args.extend(extra_args.iter().map(Into::into));

    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::execute(args, None, &mut out, &mut err);
    (
        i32::from(code),
        String::from_utf8_lossy(&out).to_string(),
        String::from_utf8_lossy(&err).to_string(),
    )
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

#[test]
fn downstream_dependency_detection() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let (code, stdout, _stderr) = run_is_affected(
        &root,
        &["src/pages/CheckoutPage.tsx"],
        &["src/components/Button.tsx"],
    );

    // Exit 0 = affected (run E2E)
    assert_eq!(
        code, 0,
        "Expected exit code 0 (affected), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("src/components/Button.tsx"));
}

#[test]
fn no_impact_unrelated_files() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let (code, stdout, _stderr) = run_is_affected(
        &root,
        &["src/pages/CheckoutPage.tsx"],
        &["src/components/Button.tsx"],
    );

    assert_eq!(code, 0);
    assert!(stdout.contains("src/components/Button.tsx"));

    let (code2, stdout2, _stderr2) = run_is_affected(
        &root,
        &["src/pages/SettingsPage.tsx"],
        &["src/utils/helpers.ts"],
    );

    // Exit 1 = not affected (skip E2E)
    assert_eq!(
        code2, 1,
        "Expected exit code 1 (not affected), got {}. stdout: {}",
        code2, stdout2
    );
    assert!(stdout2.contains("No reachability impact detected"));
}

#[test]
fn upstream_dependency_detection() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let (code, stdout, _stderr) = run_is_affected(
        &root,
        &["src/components/Button.tsx"],
        &["src/pages/CheckoutPage.tsx"],
    );

    assert_eq!(
        code, 0,
        "Expected exit code 0 (upstream impact), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("src/pages/CheckoutPage.tsx"));
}

#[test]
fn multiple_anchors() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let (code, stdout, _stderr) = run_is_affected(
        &root,
        &["src/pages/CheckoutPage.tsx", "src/pages/SettingsPage.tsx"],
        &["src/components/Button.tsx"],
    );

    assert_eq!(
        code, 0,
        "Expected exit code 0 (impact on at least one anchor), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("src/components/Button.tsx"));
}

#[test]
fn multiple_anchors_no_impact() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    fs::write(
        root.join("src/unrelated.ts"),
        r#"export const unrelated = "test";"#,
    )
    .unwrap();

    let (code, stdout, _stderr) = run_is_affected(
        &root,
        &["src/pages/CheckoutPage.tsx", "src/pages/SettingsPage.tsx"],
        &["src/unrelated.ts"],
    );

    assert_eq!(
        code, 1,
        "Expected exit code 1 (no impact), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("No reachability impact detected"));
}

#[test]
fn relative_paths() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let (code, stdout, _stderr) = run_is_affected(
        &root,
        &["src/pages/CheckoutPage.tsx"],
        &["src/components/Button.tsx"],
    );

    assert_eq!(code, 0);
    assert!(stdout.contains("src/components/Button.tsx"));
}

#[test]
fn dynamic_imports() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    fs::write(
        root.join("src/pages/LazyPage.tsx"),
        r#"const LazyComponent = () => import("../components/Card");
export const LazyPage = () => <div />;"#,
    )
    .unwrap();

    let (code, stdout, _stderr) = run_is_affected(
        &root,
        &["src/pages/LazyPage.tsx"],
        &["src/components/Card.tsx"],
    );

    assert_eq!(
        code, 0,
        "Expected exit code 0 (dynamic import detected), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("src/components/Card.tsx"));
}

#[test]
fn export_from() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    fs::write(
        root.join("src/components/index.ts"),
        r#"export { Button } from "./Button";
export { Card } from "./Card";"#,
    )
    .unwrap();

    fs::write(
        root.join("src/pages/IndexPage.tsx"),
        r#"import { Button, Card } from "../components";
export const IndexPage = () => <> <Button /> <Card /> </>;"#,
    )
    .unwrap();

    let (code, stdout, _stderr) = run_is_affected(
        &root,
        &["src/pages/IndexPage.tsx"],
        &["src/components/Button.tsx"],
    );

    assert_eq!(
        code, 0,
        "Expected exit code 0 (export-from detected), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("src/components/Button.tsx"));
}

#[test]
fn imported_image_is_affected() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    fs::write(
        root.join("src/components/Logo.tsx"),
        r#"import logo from "../assets/logo.png";
export const Logo = () => <img src={logo} />;"#,
    )
    .unwrap();

    fs::write(
        root.join("src/pages/LogoPage.tsx"),
        r#"import { Logo } from "../components/Logo";
export const LogoPage = () => <Logo />;"#,
    )
    .unwrap();

    let (code, stdout, _stderr) =
        run_is_affected(&root, &["src/pages/LogoPage.tsx"], &["src/assets/logo.png"]);

    assert_eq!(
        code, 0,
        "Expected exit code 0 (imported image affected), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("src/assets/logo.png"));
}

#[test]
fn required_asset_is_affected() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    fs::write(
        root.join("src/pages/SoundPage.tsx"),
        r#"const beep = require("../assets/beep.mp3");
export const SoundPage = () => <audio src={beep} />;"#,
    )
    .unwrap();

    let (code, stdout, _stderr) = run_is_affected(
        &root,
        &["src/pages/SoundPage.tsx"],
        &["src/assets/beep.mp3"],
    );

    assert_eq!(
        code, 0,
        "Expected exit code 0 (required asset affected), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("src/assets/beep.mp3"));
}

#[test]
fn asset_import_with_resource_query() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    fs::write(
        root.join("src/pages/QueryPage.tsx"),
        r#"import logoUrl from "../assets/logo.png?url";
import Icon from "../assets/icon.svg?react";
import "../assets/theme.css";
export const QueryPage = () => <img src={logoUrl} />;"#,
    )
    .unwrap();

    let (code, stdout, _stderr) = run_is_affected(
        &root,
        &["src/pages/QueryPage.tsx"],
        &["src/assets/icon.svg"],
    );

    assert_eq!(
        code, 0,
        "Expected exit code 0 (asset behind a resource query), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("src/assets/icon.svg"));

    let (code2, stdout2, _stderr2) = run_is_affected(
        &root,
        &["src/pages/QueryPage.tsx"],
        &["src/assets/logo.png"],
    );

    assert_eq!(
        code2, 0,
        "Expected exit code 0 (png behind ?url), got {}. stdout: {}",
        code2, stdout2
    );
    assert!(stdout2.contains("src/assets/logo.png"));
}

#[test]
fn asset_import_with_inline_loader() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    fs::write(
        root.join("src/pages/LoaderPage.tsx"),
        r#"import logo from "!!file-loader!../assets/logo.png";
export const LoaderPage = () => <img src={logo} />;"#,
    )
    .unwrap();

    let (code, stdout, _stderr) = run_is_affected(
        &root,
        &["src/pages/LoaderPage.tsx"],
        &["src/assets/logo.png"],
    );

    assert_eq!(
        code, 0,
        "Expected exit code 0 (webpack inline loader), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("src/assets/logo.png"));
}

#[test]
fn asset_referenced_via_new_url() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    fs::write(
        root.join("src/pages/UrlPage.tsx"),
        r#"const beep = new URL("../assets/beep.mp3", import.meta.url);
export const UrlPage = () => <audio src={beep.href} />;"#,
    )
    .unwrap();

    let (code, stdout, _stderr) =
        run_is_affected(&root, &["src/pages/UrlPage.tsx"], &["src/assets/beep.mp3"]);

    assert_eq!(
        code, 0,
        "Expected exit code 0 (new URL asset reference), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("src/assets/beep.mp3"));
}

#[test]
fn unimported_asset_has_no_impact() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    fs::write(
        root.join("src/components/Logo.tsx"),
        r#"import logo from "../assets/logo.png";
export const Logo = () => <img src={logo} />;"#,
    )
    .unwrap();

    // CheckoutPage reaches Card/Button, never Logo — so the image is out of its graph.

    let (code, stdout, _stderr) = run_is_affected(
        &root,
        &["src/pages/CheckoutPage.tsx"],
        &["src/assets/logo.png"],
    );

    assert_eq!(
        code, 1,
        "Expected exit code 1 (asset outside the anchor graph), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("No reachability impact detected"));
}

#[test]
fn asset_does_not_bridge_unrelated_graphs() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    // An SVG is readable text; parsing it as JS must not invent dependencies. Both
    // pages import the same icon, but SettingsPage stays unreachable from IconPage.
    fs::write(
        root.join("src/pages/IconPage.tsx"),
        r#"import icon from "../assets/icon.svg";
export const IconPage = () => <img src={icon} />;"#,
    )
    .unwrap();

    let (code, stdout, _stderr) = run_is_affected(
        &root,
        &["src/pages/IconPage.tsx"],
        &["src/components/Button.tsx"],
    );

    assert_eq!(
        code, 1,
        "Expected exit code 1 (no path through the asset), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("No reachability impact detected"));
}

#[test]
fn worker_entry_and_its_dependencies_are_affected() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    fs::create_dir_all(root.join("src/workers")).unwrap();

    // The worker URL is a `new URL` nested inside another `new` expression, so the
    // extractor only sees it by walking into the outer call's arguments.
    fs::write(
        root.join("src/pages/WorkerPage.tsx"),
        r#"const worker = new Worker(new URL("../workers/heavy.worker.ts", import.meta.url), { type: "module" });
export const WorkerPage = () => <div />;"#,
    ).unwrap();

    fs::write(
        root.join("src/workers/heavy.worker.ts"),
        r#"import { formatDate } from "../utils/helpers";
import logo from "../assets/logo.png";
onmessage = () => formatDate(logo);"#,
    )
    .unwrap();

    let (code, stdout, _stderr) = run_is_affected(
        &root,
        &["src/pages/WorkerPage.tsx"],
        &["src/workers/heavy.worker.ts"],
    );

    assert_eq!(
        code, 0,
        "Expected exit code 0 (worker entry point), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("src/workers/heavy.worker.ts"));

    // The worker is a source file, so the graph continues through it.
    let (code2, stdout2, _stderr2) = run_is_affected(
        &root,
        &["src/pages/WorkerPage.tsx"],
        &["src/utils/helpers.ts"],
    );

    assert_eq!(
        code2, 0,
        "Expected exit code 0 (module imported by a worker), got {}. stdout: {}",
        code2, stdout2
    );
    assert!(stdout2.contains("src/utils/helpers.ts"));

    let (code3, stdout3, _stderr3) = run_is_affected(
        &root,
        &["src/pages/WorkerPage.tsx"],
        &["src/assets/logo.png"],
    );

    assert_eq!(
        code3, 0,
        "Expected exit code 0 (asset imported by a worker), got {}. stdout: {}",
        code3, stdout3
    );
    assert!(stdout3.contains("src/assets/logo.png"));
}

#[test]
fn only_downstream_ignores_upstream_usages() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    // CheckoutPage imports Button (via Card), so this is a downstream hit.
    let (code, stdout, _stderr) = run_is_affected_with(
        &root,
        &["src/pages/CheckoutPage.tsx"],
        &["src/components/Button.tsx"],
        &["--only", "downstream"],
    );

    assert_eq!(
        code, 0,
        "Expected exit code 0 (downstream hit), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("src/components/Button.tsx"));

    // Reversed, the only path is upstream. Without the flag this is a hit...
    let (both, stdout_both, _stderr) = run_is_affected(
        &root,
        &["src/components/Button.tsx"],
        &["src/pages/CheckoutPage.tsx"],
    );

    assert_eq!(
        both, 0,
        "Expected exit code 0 searching both directions, got {}. stdout: {}",
        both, stdout_both
    );

    // ...and with it, the upstream path is not searched at all.
    let (code2, stdout2, _stderr2) = run_is_affected_with(
        &root,
        &["src/components/Button.tsx"],
        &["src/pages/CheckoutPage.tsx"],
        &["--only", "downstream"],
    );

    assert_eq!(
        code2, 1,
        "Expected exit code 1 (upstream path skipped), got {}. stdout: {}",
        code2, stdout2
    );
    assert!(stdout2.contains("No reachability impact detected"));
}

#[test]
fn only_upstream_ignores_downstream_usages() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    // CheckoutPage imports Button, so this is reachable downstream but not upstream.
    let (code, stdout, _stderr) = run_is_affected_with(
        &root,
        &["src/pages/CheckoutPage.tsx"],
        &["src/components/Button.tsx"],
        &["--only", "upstream"],
    );

    assert_eq!(
        code, 1,
        "Expected exit code 1 (downstream path skipped), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("No reachability impact detected"));

    let (code2, stdout2, _stderr2) = run_is_affected_with(
        &root,
        &["src/components/Button.tsx"],
        &["src/pages/CheckoutPage.tsx"],
        &["--only", "upstream"],
    );

    assert_eq!(
        code2, 0,
        "Expected exit code 0 (upstream hit), got {}. stdout: {}",
        code2, stdout2
    );
    assert!(stdout2.contains("src/pages/CheckoutPage.tsx"));
}

/// A config in a subtree the run never enters cannot have changed the answer, so it
/// is not this run's business to fail on it.
#[test]
fn unread_config_does_not_fail_the_run() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_test_project(&root);

    fs::create_dir_all(root.join("src/elsewhere")).unwrap();
    fs::write(
        root.join("src/elsewhere/fallout.toml"),
        "inline-requires = 3\n",
    )
    .unwrap();

    let (code, stdout, stderr) = run_is_affected_with(
        &root,
        &["src/components/Card.tsx"],
        &["src/components/Button.tsx"],
        &["--granularity", "symbol"],
    );

    assert_eq!(code, 0, "still a verdict: {stdout}{stderr}");
}

#[test]
fn unresolved_lists_what_named_no_file() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_unresolved_project(&root);

    // A change the anchor cannot reach, so the search walks the whole graph rather
    // than stopping at the first hit. The report covers what the run reached.
    let (_, stdout, _) = run_is_affected_with(
        &root,
        &["src/pages/LostPage.tsx"],
        &["src/components/Button.tsx"],
        &["--granularity", "symbol", "--unresolved"],
    );

    assert!(
        stdout.contains("./nowhere"),
        "the specifier that named nothing is listed: {stdout}"
    );
    assert!(
        stdout.contains("src/utils/lost.ts"),
        "and the file that wrote it: {stdout}"
    );
    assert!(
        stdout.contains("./missing"),
        "a stylesheet's specifier counts too: {stdout}"
    );
}

/// A Node builtin and a `sass:` module are answers, not failures.
#[test]
fn unresolved_leaves_out_what_names_no_file_by_design() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_unresolved_project(&root);

    let (_, stdout, _) = run_is_affected_with(
        &root,
        &["src/pages/LostPage.tsx"],
        &["src/components/Button.tsx"],
        &["--granularity", "symbol", "--unresolved"],
    );

    for named in ["\"fs\"", "  fs\n", "node:fs/promises", "sass:math"] {
        assert!(
            !stdout.contains(named),
            "{named} names no file and is not a failure: {stdout}"
        );
    }
}

/// A star whose path names no file has no file behind it for either level to
/// reach, and is reported at both. The name it could have provided still reaches
/// the page through the barrel, and a change to another module the barrel's stars
/// name still does too.
#[test]
fn a_star_that_names_no_file_is_dropped_and_reported_at_both_levels() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/page.ts"),
        "import { x } from \"./barrel\";\nexport const page = x;\n",
    )
    .unwrap();
    fs::write(
        root.join("src/barrel.ts"),
        "export * from \"./missing\";\nexport * from \"./other\";\nexport const local = 0;\n",
    )
    .unwrap();
    fs::write(root.join("src/other.ts"), "export const y = 1;\n").unwrap();

    for granularity in ["file", "symbol"] {
        let (code, stdout, stderr) = run_is_affected_with(
            &root,
            &["src/page.ts"],
            &["src/other.ts"],
            &["--granularity", granularity, "--unresolved"],
        );
        assert_eq!(code, 0, "{granularity}: {stdout}{stderr}");
        assert!(stdout.contains("./missing"), "{granularity}: {stdout}");
    }
}

#[test]
fn resolve_conditions_and_main_fields_come_from_fallout_toml() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_test_project(&root);
    setup_bundler_package(&root);
    fs::write(
        root.join("src/pages/ConditionPage.tsx"),
        "import { dual } from 'dual/conditioned';\nexport const ConditionPage = () => dual;\n",
    )
    .unwrap();

    // Without the setting the `default` entry is what resolves, which imports
    // nothing of the app's.
    let (code, stdout, _) = run_is_affected(
        &root,
        &["src/pages/ConditionPage.tsx"],
        &["src/native/exported.ts"],
    );
    assert_eq!(code, 1, "{stdout}");

    fs::write(
        root.join("fallout.toml"),
        "[resolve]\nconditions = [\"react-native\", \"import\", \"require\"]\nmain-fields = [\"react-native\", \"main\"]\n",
    )
    .unwrap();
    let (code, stdout, _) = run_is_affected(
        &root,
        &["src/pages/ConditionPage.tsx"],
        &["src/native/exported.ts"],
    );
    assert_eq!(code, 0, "the react-native export: {stdout}");

    // `dual` itself has an `exports` entry, so the main fields are not consulted for
    // it; a package without one is where they matter.
    let plain = root.join("node_modules/plain");
    fs::create_dir_all(&plain).unwrap();
    fs::write(
        plain.join("package.json"),
        r#"{ "name": "plain", "main": "./main.js", "react-native": "./native.js" }"#,
    )
    .unwrap();
    fs::write(plain.join("main.js"), "export const plain = 1;\n").unwrap();
    fs::write(
        plain.join("native.js"),
        "import '../../src/native/field.ts';\nexport const plain = 2;\n",
    )
    .unwrap();
    fs::write(
        root.join("src/pages/FieldPage.tsx"),
        "import { plain } from 'plain';\nexport const FieldPage = () => plain;\n",
    )
    .unwrap();
    let (code, stdout, _) = run_is_affected(
        &root,
        &["src/pages/FieldPage.tsx"],
        &["src/native/field.ts"],
    );
    assert_eq!(code, 0, "the react-native field: {stdout}");
}

#[test]
fn a_package_exporting_only_import_and_require_resolves_by_default() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_test_project(&root);
    setup_bundler_package(&root);
    fs::write(
        root.join("src/pages/DualPage.tsx"),
        "import { dual } from 'dual';\nexport const DualPage = () => dual;\n",
    )
    .unwrap();

    let (_, stdout, _) = run_is_affected_with(
        &root,
        &["src/pages/DualPage.tsx"],
        &["src/utils/helpers.ts"],
        &["--unresolved"],
    );
    assert!(
        !stdout.contains("  dual"),
        "`dual` offers only import and require, and resolves: {stdout}"
    );
}

#[test]
fn each_app_is_answered_with_its_own_bundler() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_test_project(&root);
    setup_bundler_package(&root);
    let plain = root.join("node_modules/plain");
    fs::create_dir_all(&plain).unwrap();
    fs::write(
        plain.join("package.json"),
        r#"{ "name": "plain", "main": "./main.js", "react-native": "./native.js" }"#,
    )
    .unwrap();
    fs::write(plain.join("main.js"), "export const plain = 1;\n").unwrap();
    fs::write(
        plain.join("native.js"),
        "import '../../src/native/field.ts';\nexport const plain = 2;\n",
    )
    .unwrap();
    for app in ["apps/mobile", "apps/web"] {
        fs::create_dir_all(root.join(app)).unwrap();
        fs::write(
            root.join(app).join("Page.tsx"),
            "import { plain } from 'plain';\nexport const Page = () => plain;\n",
        )
        .unwrap();
    }
    fs::write(
        root.join("apps/mobile/fallout.toml"),
        "[resolve]\nmain-fields = [\"react-native\", \"main\"]\n",
    )
    .unwrap();

    let (code, stdout, stderr) = run_is_affected_with(
        &root,
        &["apps/mobile/Page.tsx", "apps/web/Page.tsx"],
        &["src/native/field.ts"],
        &["--json"],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        stdout.contains(r#"{"anchor":"apps/mobile/Page.tsx","affected":true"#),
        "Metro reads the react-native field: {stdout}"
    );
    assert!(
        stdout.contains(r#"{"anchor":"apps/web/Page.tsx","affected":false"#),
        "a web bundler reads main: {stdout}"
    );

    // Asked together, the set is affected, which the mobile app is.
    let (code, stdout, _) = run_is_affected(
        &root,
        &["apps/web/Page.tsx", "apps/mobile/Page.tsx"],
        &["src/native/field.ts"],
    );
    assert_eq!(code, 0, "{stdout}");
}

#[test]
fn json_answers_each_anchor_and_classes_what_it_could_not_place() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_test_project(&root);
    setup_bundler_package(&root);
    fs::create_dir_all(root.join("src/app")).unwrap();
    fs::write(
        root.join("fallout.toml"),
        "[aliases]\n\"@app\" = \"src/app\"\n",
    )
    .unwrap();
    fs::write(
        root.join("tsconfig.json"),
        r#"{ "compilerOptions": { "paths": { "~/*": ["./src/*"] } } }"#,
    )
    .unwrap();
    fs::write(
        root.join("src/pages/GapPage.tsx"),
        r##"import "./gone";
import "@app/gone";
import "~/gone";
import "#internal";
import "dual/unexported";
import "not-installed";
export const GapPage = () => 1;
"##,
    )
    .unwrap();

    let (code, stdout, stderr) = run_is_affected_with(
        &root,
        &["src/pages/GapPage.tsx", "src/pages/CheckoutPage.tsx"],
        &["src/components/Button.tsx"],
        &["--json"],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        stdout.starts_with(r#"{"anchors":[{"anchor":"src/pages/GapPage.tsx","affected":false"#),
        "anchors come back in the order given: {stdout}"
    );
    for (specifier, kind, in_repo) in [
        ("./gone", "path", true),
        ("@app/gone", "alias", true),
        ("~/gone", "alias", true),
        ("#internal", "alias", true),
        ("dual/unexported", "package", true),
        ("not-installed", "missing-package", false),
    ] {
        let entry = format!(
            r#"{{"specifier":"{specifier}","kind":"{kind}","in_repo":{in_repo},"from":["src/pages/GapPage.tsx"]}}"#
        );
        assert!(stdout.contains(&entry), "{entry} in {stdout}");
    }
    assert!(
        stdout.contains(r#"{"anchor":"src/pages/CheckoutPage.tsx","affected":true,"direction":"downstream","changed":"src/components/Button.tsx""#),
        "{stdout}"
    );
}

#[test]
fn json_classes_a_specifier_by_its_most_in_repo_writer() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_test_project(&root);
    fs::create_dir_all(root.join("apps/web/src")).unwrap();
    fs::create_dir_all(root.join("shared")).unwrap();
    fs::write(
        root.join("apps/web/fallout.toml"),
        "[aliases]\n\"~\" = \"src\"\n",
    )
    .unwrap();
    // The same missing name, written in the app, where it is an alias, and in a
    // shared file, where nothing says so.
    fs::write(
        root.join("apps/web/page.tsx"),
        "import \"~/theme\";\nimport \"../../shared/util\";\nexport const Page = () => 1;\n",
    )
    .unwrap();
    fs::write(
        root.join("shared/util.ts"),
        "import \"~/theme\";\nexport const util = 1;\n",
    )
    .unwrap();

    let (code, stdout, stderr) = run_is_affected_with(
        &root,
        &["apps/web/page.tsx"],
        &["src/components/Button.tsx"],
        &["--json"],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        stdout.contains(r#"{"specifier":"~/theme","kind":"alias","in_repo":true"#),
        "{stdout}"
    );
}

#[test]
fn json_lists_only_what_an_anchors_own_bundler_could_not_place() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_test_project(&root);
    let package = root.join("node_modules/native-only");
    fs::create_dir_all(&package).unwrap();
    fs::write(
        package.join("package.json"),
        r#"{ "name": "native-only", "exports": { ".": { "react-native": "./native.js" } } }"#,
    )
    .unwrap();
    fs::write(package.join("native.js"), "export const a = 1;\n").unwrap();
    for app in ["apps/mobile", "apps/web"] {
        fs::create_dir_all(root.join(app)).unwrap();
        fs::write(
            root.join(app).join("Page.tsx"),
            "import { a } from 'native-only';\nexport const Page = () => a;\n",
        )
        .unwrap();
    }
    fs::write(
        root.join("apps/mobile/fallout.toml"),
        "[resolve]\nconditions = [\"react-native\"]\n",
    )
    .unwrap();

    let (code, stdout, stderr) = run_is_affected_with(
        &root,
        &["apps/web/Page.tsx", "apps/mobile/Page.tsx"],
        &["src/components/Button.tsx"],
        &["--json"],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        stdout.contains(r#"{"anchor":"apps/mobile/Page.tsx","affected":false,"granularity":"file","unresolved":[]}"#),
        "the mobile bundler placed it: {stdout}"
    );
    assert!(
        stdout.contains(r#"{"specifier":"native-only","kind":"package""#),
        "the web bundler did not: {stdout}"
    );
}

#[test]
fn json_classes_stylesheet_names_and_workspace_packages() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_test_project(&root);
    fs::create_dir_all(root.join("node_modules/bootstrap")).unwrap();
    fs::create_dir_all(root.join("packages/ui")).unwrap();
    fs::write(
        root.join("packages/ui/package.json"),
        "{\n  \"name\": \"@acme/ui\",\n  \"dependencies\": { \"name\": \"not this one\" }\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/pages/styled.scss"),
        "@use \"mixins\";\n@use \"~bootstrap/scss/gone\";\n",
    )
    .unwrap();
    fs::write(
        root.join("src/pages/StyledPage.tsx"),
        "import \"./styled.scss\";\nimport \"@acme/ui\";\nexport const StyledPage = () => 1;\n",
    )
    .unwrap();

    let (code, stdout, stderr) = run_is_affected_with(
        &root,
        &["src/pages/StyledPage.tsx"],
        &["src/components/Button.tsx"],
        &["--json"],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
    for (specifier, kind) in [
        ("mixins", "path"),
        ("~bootstrap/scss/gone", "package"),
        ("@acme/ui", "package"),
    ] {
        let entry = format!(r#"{{"specifier":"{specifier}","kind":"{kind}","in_repo":true"#);
        assert!(stdout.contains(&entry), "{entry} in {stdout}");
    }
}

#[test]
fn a_config_extended_from_a_package_is_read_as_its_tsconfig() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    // The package has code of its own, so resolving its name as a module finds
    // `index.ts` rather than the config it ships.
    let package = root.join("node_modules/@acme/tsconfig");
    fs::create_dir_all(&package).unwrap();
    fs::write(
        package.join("package.json"),
        r#"{ "name": "@acme/tsconfig", "version": "1.0.0" }"#,
    )
    .unwrap();
    fs::write(package.join("index.ts"), "export const preset = 1;\n").unwrap();
    fs::write(
        package.join("tsconfig.json"),
        r#"{ "compilerOptions": { "baseUrl": "." } }"#,
    )
    .unwrap();
    fs::write(
        root.join("tsconfig.json"),
        r#"{ "extends": "@acme/tsconfig" }"#,
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/theme.ts"), "export const theme = 1;\n").unwrap();
    fs::write(
        root.join("src/page.ts"),
        "import { theme } from \"theme\";\nexport const page = theme;\n",
    )
    .unwrap();

    let (code, stdout, stderr) = run_is_affected(
        &root,
        &["src/page.ts"],
        &["node_modules/@acme/tsconfig/tsconfig.json"],
    );
    assert_eq!(code, 0, "{stdout}{stderr}");
}

/// The answer for `src/page.ts` in `root` when `changed` changed, at
/// `granularity`, with `earlier` (paths relative to `root`) standing in for the
/// versions a base revision would hold.
fn page_answer(
    root: &Path,
    changed: &str,
    granularity: Granularity,
    earlier: Option<&[(&str, &str)]>,
) -> Result<fallout::Outcome, fallout::Error> {
    let options = Options {
        anchors: vec![PathBuf::from("src/page.ts")],
        changed: vec![PathBuf::from(changed)],
        diff: None,
        base: earlier.map(|_| "HEAD".to_string()),
        root: root.to_path_buf(),
        only: None,
        granularity,
        include_types: false,
    };
    let earlier = earlier.map(|files| -> Box<dyn Earlier> {
        Box::new(
            files
                .iter()
                .map(|(path, text)| (root.join(path), text.to_string()))
                .collect::<AHashMap<PathBuf, String>>(),
        )
    });
    analyse_with(&options, earlier)
}

/// Whether `answer` is no answer, for the tsconfig at `path`.
fn names_unreadable_tsconfig(
    answer: &Result<fallout::Outcome, fallout::Error>,
    path: &Path,
) -> bool {
    matches!(answer, Err(fallout::Error::UnreadableTsconfig { path: named, .. }) if named == path)
}

/// Read, the tsconfig makes the changed config it extends govern the page, and the
/// import goes where the mapping says.
#[test]
fn a_readable_tsconfig_governs_the_file_and_maps_its_imports() {
    let temp = TempDir::new().unwrap();
    let root = fallout::canonical_root(temp.path());
    setup_tsconfig_project(&root);
    for granularity in [Granularity::File, Granularity::Symbol] {
        for changed in ["tsconfig.base.json", "src/ui/button.ts"] {
            let answer = page_answer(&root, changed, granularity, None).expect("an answer");
            assert!(answer.verdict.is_affected(), "{changed} {granularity:?}");
        }
    }
}

/// What a tsconfig that cannot be read maps is not known, so there is no answer:
/// with the mapping unread, a changed config it extends would govern nothing and
/// `@ui/button` would land on the installed package.
#[cfg(unix)]
#[test]
fn an_unreadable_tsconfig_is_no_answer() {
    let temp = TempDir::new().unwrap();
    let root = fallout::canonical_root(temp.path());
    setup_tsconfig_project(&root);
    let tsconfig = root.join("src/tsconfig.json");
    let Some(_unreadable) = Unreadable::make(tsconfig.clone()) else {
        eprintln!("skipped: permissions do not stop this process reading files");
        return;
    };
    // With a base revision the mapping is known to have moved: `@ui/*` named
    // `src/legacy` before.
    let earlier = [(
        "tsconfig.base.json",
        r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@ui/*": ["src/legacy/*"] } } }"#,
    )];
    for changed in ["tsconfig.base.json", "src/ui/button.ts"] {
        for granularity in [Granularity::File, Granularity::Symbol] {
            let answer = page_answer(&root, changed, granularity, None);
            assert!(
                names_unreadable_tsconfig(&answer, &tsconfig),
                "{changed} {granularity:?}: {answer:?}"
            );
        }
        let answer = page_answer(&root, changed, Granularity::Symbol, Some(&earlier));
        assert!(
            names_unreadable_tsconfig(&answer, &tsconfig),
            "{changed} base: {answer:?}"
        );
    }

    // The unresolved report is no answer too, rather than a list with the import
    // missing from it.
    for granularity in ["file", "symbol"] {
        let (code, stdout, stderr) = run_is_affected_with(
            &root,
            &["src/page.ts"],
            &["src/ui/button.ts"],
            &["--granularity", granularity, "--unresolved"],
        );
        assert_eq!(code, 2, "{granularity}: {stdout}{stderr}");
    }
}

/// The resolver skips a tsconfig whose `extends` it cannot read along with the
/// tsconfig itself, so that is no answer too, naming the config it could not read.
#[cfg(unix)]
#[test]
fn an_unreadable_extended_config_is_no_answer() {
    let temp = TempDir::new().unwrap();
    let root = fallout::canonical_root(temp.path());
    setup_tsconfig_project(&root);
    let base = root.join("tsconfig.base.json");
    let Some(_unreadable) = Unreadable::make(base.clone()) else {
        eprintln!("skipped: permissions do not stop this process reading files");
        return;
    };
    for granularity in [Granularity::File, Granularity::Symbol] {
        let answer = page_answer(&root, "src/ui/button.ts", granularity, None);
        assert!(
            names_unreadable_tsconfig(&answer, &base),
            "{granularity:?}: {answer:?}"
        );
    }
}

/// A tsconfig above the file can decide through `references` which config is
/// found, so a changed config with no earlier text governs the file if one it
/// references does. One it references that cannot be read is no answer: whether it
/// reads the changed config is exactly what is not known.
#[cfg(unix)]
#[test]
fn an_unreadable_config_a_tsconfig_above_references_is_no_answer() {
    let temp = TempDir::new().unwrap();
    let root = fallout::canonical_root(temp.path());
    setup_tsconfig_project(&root);
    fs::write(
        root.join("tsconfig.json"),
        r#"{ "references": [{ "path": "./tools" }] }"#,
    )
    .unwrap();
    fs::create_dir_all(root.join("tools")).unwrap();
    let tools = root.join("tools/tsconfig.json");
    fs::write(&tools, r#"{ "extends": "../tsconfig.shared.json" }"#).unwrap();
    fs::write(root.join("tsconfig.shared.json"), "{}").unwrap();
    let Some(_unreadable) = Unreadable::make(tools.clone()) else {
        eprintln!("skipped: permissions do not stop this process reading files");
        return;
    };
    for granularity in [Granularity::File, Granularity::Symbol] {
        let answer = page_answer(&root, "tsconfig.shared.json", granularity, None);
        assert!(
            names_unreadable_tsconfig(&answer, &tools),
            "{granularity:?}: {answer:?}"
        );
    }
}

/// The resolver stops at the first tsconfig it can read above a file, even when
/// what it hands back is a project that tsconfig references from elsewhere. A
/// tsconfig above that one is never consulted, so one that cannot be read there is
/// no reason to refuse an answer.
#[cfg(unix)]
#[test]
fn an_unreadable_tsconfig_above_the_one_found_does_not_refuse_an_answer() {
    let temp = TempDir::new().unwrap();
    let outer = fallout::canonical_root(temp.path());
    let root = outer.join("project");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("config/app")).unwrap();
    fs::write(
        root.join("tsconfig.json"),
        r#"{ "files": [], "references": [{ "path": "./config/app" }] }"#,
    )
    .unwrap();
    fs::write(
        root.join("config/app/tsconfig.json"),
        r#"{ "compilerOptions": { "composite": true }, "include": ["../../src/**/*"] }"#,
    )
    .unwrap();
    fs::write(root.join("src/theme.ts"), "export const theme = 1;\n").unwrap();
    fs::write(
        root.join("src/page.ts"),
        "import { theme } from \"./theme\";\nexport const page = theme;\n",
    )
    .unwrap();
    let above = outer.join("tsconfig.json");
    fs::write(&above, "{}").unwrap();
    let Some(_unreadable) = Unreadable::make(above) else {
        eprintln!("skipped: permissions do not stop this process reading files");
        return;
    };
    for granularity in [Granularity::File, Granularity::Symbol] {
        let answer = page_answer(&root, "src/theme.ts", granularity, None);
        assert!(
            answer
                .as_ref()
                .is_ok_and(|answer| answer.verdict.is_affected()),
            "{granularity:?}: {answer:?}"
        );
    }
}

/// A readable tsconfig that does not claim the file, one with `"files": []` and no
/// reference that does, is walked past by the resolver on its way to the next one
/// up. One that cannot be read there would have been the file's, so it is no
/// answer.
#[cfg(unix)]
#[test]
fn an_unreadable_tsconfig_above_one_that_does_not_claim_the_file_is_no_answer() {
    let temp = TempDir::new().unwrap();
    let root = fallout::canonical_root(temp.path());
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/tsconfig.json"), r#"{ "files": [] }"#).unwrap();
    fs::write(root.join("src/theme.ts"), "export const theme = 1;\n").unwrap();
    fs::write(
        root.join("src/page.ts"),
        "import { theme } from \"./theme\";\nexport const page = theme;\n",
    )
    .unwrap();
    let above = root.join("tsconfig.json");
    fs::write(&above, r#"{ "include": ["src/**/*"] }"#).unwrap();
    let Some(_unreadable) = Unreadable::make(above.clone()) else {
        eprintln!("skipped: permissions do not stop this process reading files");
        return;
    };
    for granularity in [Granularity::File, Granularity::Symbol] {
        let answer = page_answer(&root, "src/theme.ts", granularity, None);
        assert!(
            names_unreadable_tsconfig(&answer, &above),
            "{granularity:?}: {answer:?}"
        );
    }
}

/// A config a tsconfig references can sit in a directory this process may not
/// enter, and then even asking whether it is there fails. That is not a config
/// that is absent: whether it reads the changed config is not known, so it is no
/// answer too.
#[cfg(unix)]
#[test]
fn a_referenced_config_in_a_directory_that_cannot_be_entered_is_no_answer() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new().unwrap();
    let root = fallout::canonical_root(temp.path());
    setup_tsconfig_project(&root);
    fs::write(
        root.join("tsconfig.json"),
        r#"{ "references": [{ "path": "./tools" }] }"#,
    )
    .unwrap();
    fs::create_dir_all(root.join("tools")).unwrap();
    let tools = root.join("tools/tsconfig.json");
    fs::write(&tools, r#"{ "extends": "../tsconfig.shared.json" }"#).unwrap();
    fs::write(root.join("tsconfig.shared.json"), "{}").unwrap();
    let directory = root.join("tools");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o000)).unwrap();
    let blocked = fs::metadata(&tools).is_err();
    let answers: Vec<_> = [Granularity::File, Granularity::Symbol]
        .into_iter()
        .map(|granularity| {
            (
                granularity,
                page_answer(&root, "tsconfig.shared.json", granularity, None),
            )
        })
        .collect();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    if !blocked {
        eprintln!("skipped: permissions do not stop this process entering directories");
        return;
    }
    for (granularity, answer) in answers {
        assert!(
            names_unreadable_tsconfig(&answer, &tools),
            "{granularity:?}: {answer:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_deleted_file_of_a_workspace_package_moves_its_deep_imports() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    // The package is linked into `node_modules` the way a workspace links it, and
    // `@acme/ui/button` names `button.ts` until it is gone, then the directory.
    let package = root.join("packages/ui");
    fs::create_dir_all(package.join("button")).unwrap();
    fs::write(package.join("package.json"), r#"{ "name": "@acme/ui" }"#).unwrap();
    fs::write(
        package.join("button/index.ts"),
        "export const button = 1;\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("node_modules/@acme")).unwrap();
    std::os::unix::fs::symlink("../../packages/ui", root.join("node_modules/@acme/ui")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/page.ts"),
        "import { button } from \"@acme/ui/button\";\nexport const page = button;\n",
    )
    .unwrap();

    let (code, stdout, stderr) =
        run_is_affected(&root, &["src/page.ts"], &["packages/ui/button.ts"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
}

/// Whether `anchor` comes out affected by `changed` at `granularity`, with no
/// earlier versions, or with `earlier` (paths relative to `root`) standing in for
/// the versions a base revision would hold.
fn affected_at(
    root: &Path,
    anchor: &str,
    changed: &[&str],
    granularity: Granularity,
    earlier: Option<&[(&str, &str)]>,
) -> bool {
    let options = Options {
        anchors: vec![PathBuf::from(anchor)],
        changed: changed.iter().map(PathBuf::from).collect(),
        diff: None,
        base: earlier.map(|_| "HEAD".to_string()),
        root: root.to_path_buf(),
        only: None,
        granularity,
        include_types: false,
    };
    let earlier = earlier.map(|files| -> Box<dyn Earlier> {
        Box::new(
            files
                .iter()
                .map(|(path, text)| (root.join(path), text.to_string()))
                .collect::<AHashMap<PathBuf, String>>(),
        )
    });
    analyse_with(&options, earlier)
        .expect("an answer")
        .verdict
        .is_affected()
}

/// A project written into a fresh directory, spelled the way the resolver spells it.
fn project(files: &[(&str, &str)]) -> (TempDir, PathBuf) {
    let temp = TempDir::new().unwrap();
    let root = fallout::canonical_root(temp.path());
    for (path, text) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    (temp, root)
}

/// A page whose stylesheet has `@use "../styles/<name>"`, with `files` besides.
fn sass_project(name: &str, files: &[(&str, &str)]) -> (TempDir, PathBuf) {
    let page_scss = format!("@use \"../styles/{name}\";\n.t {{ color: red; }}\n");
    let mut all = vec![
        (
            "src/pages/Page.tsx",
            "import \"./page.scss\";\nexport const Page = () => <h1 className=\"t\">Hi</h1>;\n",
        ),
        ("src/pages/page.scss", page_scss.as_str()),
    ];
    all.extend_from_slice(files);
    project(&all)
}

/// Whether a change to `changed` reaches the page. Both levels are asked, and have
/// to agree: a stylesheet is one node either way.
fn sass_reaches(root: &Path, changed: &str) -> bool {
    let at = |granularity| affected_at(root, "src/pages/Page.tsx", &[changed], granularity, None);
    let file = at(Granularity::File);
    assert_eq!(
        file,
        at(Granularity::Symbol),
        "{changed}: the levels disagree"
    );
    file
}

/// Sass resolves a URL for extensions and partials before it tries `url/index`, so
/// `@use "../styles/theme"` loads `_theme.scss` with `theme/_index.scss` beside it.
#[test]
fn a_sass_partial_is_found_before_a_directory_index() {
    let (_keep, root) = sass_project(
        "theme",
        &[
            ("src/styles/_theme.scss", "$ink: red;\n"),
            ("src/styles/theme/_index.scss", "$ink: green;\n"),
        ],
    );
    assert!(sass_reaches(&root, "src/styles/_theme.scss"));
    let earlier = [("src/styles/_theme.scss", "$ink: blue;\n")];
    assert!(
        affected_at(
            &root,
            "src/pages/Page.tsx",
            &["src/styles/_theme.scss"],
            Granularity::Symbol,
            Some(&earlier)
        ),
        "base"
    );
    // The index is the one Sass never loads, so an edit to it reaches nothing.
    assert!(!sass_reaches(&root, "src/styles/theme/_index.scss"));
}

/// The tree before the change is asked by the same rules. Deleting the partial
/// sends the same URL to the index, so the import moved.
#[test]
fn deleting_a_sass_partial_moves_its_url_to_the_directory_index() {
    let (_keep, root) = sass_project(
        "theme",
        &[("src/styles/theme/_index.scss", "$ink: green;\n")],
    );
    assert!(sass_reaches(&root, "src/styles/_theme.scss"));
}

/// `.css` is tried only when neither `.sass` nor `.scss` resolves, as a partial or
/// not, so Sass takes `_tokens.scss` over `tokens.css`.
#[test]
fn a_sass_partial_is_found_before_a_css_file() {
    let (_keep, root) = sass_project(
        "tokens",
        &[
            ("src/styles/_tokens.scss", "$ink: red;\n"),
            ("src/styles/tokens.css", ".unused { color: blue; }\n"),
        ],
    );
    assert!(sass_reaches(&root, "src/styles/_tokens.scss"));
    assert!(!sass_reaches(&root, "src/styles/tokens.css"));
}

/// Where `theme.scss` and `_theme.scss` are both there, Sass refuses the URL.
/// Which one the author meant is not known, so both are kept.
#[test]
fn an_ambiguous_sass_url_keeps_every_candidate() {
    let (_keep, root) = sass_project(
        "theme",
        &[
            ("src/styles/theme.scss", "$ink: red;\n"),
            ("src/styles/_theme.scss", "$ink: green;\n"),
        ],
    );
    assert!(sass_reaches(&root, "src/styles/theme.scss"));
    assert!(sass_reaches(&root, "src/styles/_theme.scss"));
}

/// With nothing for the URL itself, the directory's index is found, as a partial.
#[test]
fn a_sass_url_naming_only_a_directory_finds_its_index() {
    let (_keep, root) = sass_project("theme", &[("src/styles/theme/_index.scss", "$ink: red;\n")]);
    assert!(sass_reaches(&root, "src/styles/theme/_index.scss"));
}

/// An object another module owns, and a function that records what it is handed.
const EFFECT_TARGETS: &str = "export const obj = { n: 0 };
export const reg = (n) => {
  globalThis.registered = n;
  return n;
};
";

/// The page imports only `sibling` from `lib.ts`, and reads `obj.n` itself. Every
/// edited statement runs something on load before and after the edit, so the page
/// is affected at every level, as it is at `file`.
#[test]
fn an_edited_statement_that_runs_something_on_load_reaches_every_importer() {
    let cases = [
        // An update, a delete, and an assignment to a name nothing declares.
        (
            "export const x = (obj.n++, 1);",
            "export const x = (obj.n++, 2);",
        ),
        (
            "export const x = (delete obj.n, 1);",
            "export const x = (delete obj.n, 2);",
        ),
        (
            "export const x = (implicitGlobal = 1);",
            "export const x = (implicitGlobal = 2);",
        ),
        (
            "export class W { static { obj.n++; } }",
            "export class W { static { obj.n++; obj.n++; } }",
        ),
        // A default in a destructuring pattern.
        (
            "export const { a = reg(1) } = obj;",
            "export const { a = reg(2) } = obj;",
        ),
        // A read before the declaration, which throws.
        (
            "export const v = [later, 1];\nconst later = 1;",
            "export const v = [later, 2];\nconst later = 1;",
        ),
    ];
    let head = "import { obj, reg } from \"./obj\";\n";
    let tail = "\nexport const sibling = 1;\n";
    let mut missed = Vec::new();
    for (before, after) in cases {
        let temp = TempDir::new().unwrap();
        let root = fallout::canonical_root(temp.path());
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/obj.ts"), EFFECT_TARGETS).unwrap();
        fs::write(
            root.join("src/page.ts"),
            "import { obj } from \"./obj\";\nimport { sibling } from \"./lib\";\n\
             export const Page = () => [sibling, obj.n];\n",
        )
        .unwrap();
        let (old, new) = (
            format!("{head}{before}{tail}"),
            format!("{head}{after}{tail}"),
        );
        fs::write(root.join("src/lib.ts"), &new).unwrap();
        // The diff CI would hand over, so a symbol run attributes the lines it names.
        let diff = similar::TextDiff::from_lines(&old, &new)
            .unified_diff()
            .context_radius(3)
            .header("a/src/lib.ts", "b/src/lib.ts")
            .to_string();

        for (level, granularity, base) in [
            ("file", Granularity::File, false),
            ("symbol", Granularity::Symbol, false),
            ("base", Granularity::Symbol, true),
        ] {
            let options = Options {
                anchors: vec![PathBuf::from("src/page.ts")],
                changed: Vec::new(),
                diff: Some(diff.clone()),
                base: base.then(|| "HEAD".to_string()),
                root: root.clone(),
                only: None,
                granularity,
                include_types: false,
            };
            let earlier = base.then(|| -> Box<dyn Earlier> {
                Box::new(AHashMap::from_iter([(
                    root.join("src/lib.ts"),
                    old.clone(),
                )]))
            });
            let answer = analyse_with(&options, earlier).expect("an answer");
            if !answer.verdict.is_affected() {
                missed.push(format!("{level}: {after}"));
            }
        }
    }
    assert!(missed.is_empty(), "not affected:\n{}", missed.join("\n"));
}
