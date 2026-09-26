use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Builds once for the whole file. Tests run in parallel and every one of them
/// wants the binary, so without this the first build replaces the executable while
/// another test is part-way through running it, which shows up as an exit code of
/// -1 and an empty stdout.
static BUILD: std::sync::Once = std::sync::Once::new();

fn build_binary() -> PathBuf {
    BUILD.call_once(|| {
        let output = Command::new("cargo")
            .args(["build", "--release"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("Failed to build binary");

        if !output.status.success() {
            panic!("Build failed: {}", String::from_utf8_lossy(&output.stderr));
        }
    });

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.join("target/release/fallout")
}

fn run_is_affected(
    binary: &Path,
    root: &Path,
    anchors: &[&str],
    changed: &[&str],
) -> (i32, String, String) {
    run_is_affected_with(binary, root, anchors, changed, &[])
}

fn run_is_affected_with(
    binary: &Path,
    root: &Path,
    anchors: &[&str],
    changed: &[&str],
    extra_args: &[&str],
) -> (i32, String, String) {
    let mut cmd = Command::new(binary);
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

fn setup_test_project(root: &Path) {
    fs::create_dir_all(root.join("src/components")).unwrap();
    fs::create_dir_all(root.join("src/utils")).unwrap();
    fs::create_dir_all(root.join("src/pages")).unwrap();
    fs::create_dir_all(root.join("src/assets")).unwrap();

    // A real PNG header, so the file is not valid UTF-8.
    fs::write(
        root.join("src/assets/logo.png"),
        [0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a],
    )
    .unwrap();

    fs::write(
        root.join("src/assets/icon.svg"),
        r#"<svg xmlns="http://www.w3.org/2000/svg"><rect /></svg>"#,
    )
    .unwrap();

    fs::write(
        root.join("src/assets/theme.css"),
        r#".button { color: red; }"#,
    )
    .unwrap();

    fs::write(root.join("src/assets/beep.mp3"), [0x49u8, 0x44, 0x33, 0x04]).unwrap();

    fs::write(
        root.join("src/components/Button.tsx"),
        r#"export const Button = () => <button>Click</button>;"#,
    )
    .unwrap();

    fs::write(
        root.join("src/components/Card.tsx"),
        r#"import { Button } from "./Button";
export const Card = () => <div><Button /></div>;"#,
    )
    .unwrap();

    fs::write(
        root.join("src/utils/helpers.ts"),
        r#"export const formatDate = (d: Date) => d.toISOString();"#,
    )
    .unwrap();

    fs::write(
        root.join("src/pages/CheckoutPage.tsx"),
        r#"import { Card } from "../components/Card";
import { formatDate } from "../utils/helpers";
export const CheckoutPage = () => <Card />;"#,
    )
    .unwrap();

    fs::write(
        root.join("src/pages/SettingsPage.tsx"),
        r#"import { Button } from "../components/Button";
export const SettingsPage = () => <Button />;"#,
    )
    .unwrap();

    fs::write(
        root.join("src/App.tsx"),
        r#"import { CheckoutPage } from "./pages/CheckoutPage";
import { SettingsPage } from "./pages/SettingsPage";
export const App = () => <> <CheckoutPage /> <SettingsPage /> </>;"#,
    )
    .unwrap();
}

#[test]
fn test_downstream_dependency_detection() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
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
fn test_no_impact_unrelated_files() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
        &root,
        &["src/pages/CheckoutPage.tsx"],
        &["src/components/Button.tsx"],
    );

    assert_eq!(code, 0);
    assert!(stdout.contains("src/components/Button.tsx"));

    let (code2, stdout2, _stderr2) = run_is_affected(
        &binary,
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
fn test_upstream_dependency_detection() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
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
fn test_multiple_anchors() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
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
fn test_multiple_anchors_no_impact() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let binary = build_binary();

    fs::write(
        root.join("src/unrelated.ts"),
        r#"export const unrelated = "test";"#,
    )
    .unwrap();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
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
fn test_relative_paths() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
        &root,
        &["src/pages/CheckoutPage.tsx"],
        &["src/components/Button.tsx"],
    );

    assert_eq!(code, 0);
    assert!(stdout.contains("src/components/Button.tsx"));
}

#[test]
fn test_dynamic_imports() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    fs::write(
        root.join("src/pages/LazyPage.tsx"),
        r#"const LazyComponent = () => import("../components/Card");
export const LazyPage = () => <div />;"#,
    )
    .unwrap();

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
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
fn test_export_from() {
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

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
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
fn test_no_anchor_error() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let binary = build_binary();

    let mut cmd = Command::new(&binary);
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
fn test_invalid_anchor_error() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let binary = build_binary();

    let (code, stdout, stderr) = run_is_affected(
        &binary,
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
fn test_imported_image_is_affected() {
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

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
        &root,
        &["src/pages/LogoPage.tsx"],
        &["src/assets/logo.png"],
    );

    assert_eq!(
        code, 0,
        "Expected exit code 0 (imported image affected), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("src/assets/logo.png"));
}

#[test]
fn test_required_asset_is_affected() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    fs::write(
        root.join("src/pages/SoundPage.tsx"),
        r#"const beep = require("../assets/beep.mp3");
export const SoundPage = () => <audio src={beep} />;"#,
    )
    .unwrap();

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
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
fn test_asset_import_with_resource_query() {
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

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
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
        &binary,
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
fn test_asset_import_with_inline_loader() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    fs::write(
        root.join("src/pages/LoaderPage.tsx"),
        r#"import logo from "!!file-loader!../assets/logo.png";
export const LoaderPage = () => <img src={logo} />;"#,
    )
    .unwrap();

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
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
fn test_asset_referenced_via_new_url() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    fs::write(
        root.join("src/pages/UrlPage.tsx"),
        r#"const beep = new URL("../assets/beep.mp3", import.meta.url);
export const UrlPage = () => <audio src={beep.href} />;"#,
    )
    .unwrap();

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
        &root,
        &["src/pages/UrlPage.tsx"],
        &["src/assets/beep.mp3"],
    );

    assert_eq!(
        code, 0,
        "Expected exit code 0 (new URL asset reference), got {}. stdout: {}",
        code, stdout
    );
    assert!(stdout.contains("src/assets/beep.mp3"));
}

#[test]
fn test_unimported_asset_has_no_impact() {
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
    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
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
fn test_asset_does_not_bridge_unrelated_graphs() {
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

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
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
fn test_worker_entry_and_its_dependencies_are_affected() {
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

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
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
        &binary,
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
        &binary,
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
fn test_only_downstream_ignores_upstream_usages() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let binary = build_binary();

    // CheckoutPage imports Button (via Card), so this is a downstream hit.
    let (code, stdout, _stderr) = run_is_affected_with(
        &binary,
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
        &binary,
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
        &binary,
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
fn test_only_upstream_ignores_downstream_usages() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let binary = build_binary();

    // CheckoutPage imports Button, so this is reachable downstream but not upstream.
    let (code, stdout, _stderr) = run_is_affected_with(
        &binary,
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
        &binary,
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

#[test]
fn test_only_short_flag_matches_long_flag() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected_with(
        &binary,
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
fn test_only_rejects_unknown_direction() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path().to_path_buf();
    setup_test_project(&root);

    let binary = build_binary();

    let (code, stdout, stderr) = run_is_affected_with(
        &binary,
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

/// A config the run needs and cannot read replaces the verdict.
///
/// Silence is the failure mode this guards against. A setting that quietly does
/// nothing shows up later as a verdict nobody can explain, and the whole point of
/// declaring it was to be believed.
#[test]
fn test_unreadable_config_is_reported_rather_than_ignored() {
    let binary = build_binary();
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_test_project(&root);

    fs::write(
        root.join("src/components/fallout.toml"),
        "inline-requires = 3\n",
    )
    .unwrap();

    let (code, stdout, stderr) = run_is_affected_with(
        &binary,
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

/// A config in a subtree the run never enters cannot have changed the answer, so it
/// is not this run's business to fail on it.
#[test]
fn test_unread_config_does_not_fail_the_run() {
    let binary = build_binary();
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
        &binary,
        &root,
        &["src/components/Card.tsx"],
        &["src/components/Button.tsx"],
        &["--granularity", "symbol"],
    );

    assert_eq!(code, 0, "still a verdict: {stdout}{stderr}");
}

/// Lays a file with a mix of specifiers over the sample project: one that names
/// nothing, and two that name no file by design.
fn setup_unresolved_project(root: &Path) {
    setup_test_project(root);

    fs::write(
        root.join("src/utils/lost.ts"),
        r#"import { gone } from "./nowhere";
import { readFile } from "fs";
import { open } from "node:fs/promises";
export const lost = () => gone(readFile, open);"#,
    )
    .unwrap();

    fs::write(
        root.join("src/pages/lost.scss"),
        "@use 'sass:math';\n@use './missing';\n.lost { width: math.div(1, 2); }\n",
    )
    .unwrap();

    fs::write(
        root.join("src/pages/LostPage.tsx"),
        r#"import { lost } from "../utils/lost";
import "./lost.scss";
export const LostPage = () => lost();"#,
    )
    .unwrap();
}

#[test]
fn test_unresolved_lists_what_named_no_file() {
    let binary = build_binary();
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_unresolved_project(&root);

    // A change the anchor cannot reach, so the search walks the whole graph rather
    // than stopping at the first hit. The report covers what the run reached.
    let (_, stdout, _) = run_is_affected_with(
        &binary,
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
fn test_unresolved_leaves_out_what_names_no_file_by_design() {
    let binary = build_binary();
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_unresolved_project(&root);

    let (_, stdout, _) = run_is_affected_with(
        &binary,
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

/// The flag reports and does not judge: same verdict, same exit code.
#[test]
fn test_unresolved_does_not_change_the_verdict() {
    let binary = build_binary();
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_unresolved_project(&root);

    let plain = run_is_affected_with(
        &binary,
        &root,
        &["src/pages/LostPage.tsx"],
        &["src/utils/lost.ts"],
        &["--granularity", "symbol"],
    );
    let listed = run_is_affected_with(
        &binary,
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

#[test]
fn test_exit_code_tells_not_affected_from_no_answer() {
    let binary = build_binary();
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_test_project(&root);

    let affected = run_is_affected(
        &binary,
        &root,
        &["src/pages/CheckoutPage.tsx"],
        &["src/components/Button.tsx"],
    );
    assert_eq!(affected.0, 0, "affected: {affected:?}");

    let unaffected = run_is_affected(
        &binary,
        &root,
        &["src/pages/SettingsPage.tsx"],
        &["src/utils/helpers.ts"],
    );
    assert_eq!(unaffected.0, 1, "not affected: {unaffected:?}");

    let missing = run_is_affected(&binary, &root, &["src/pages/Nope.tsx"], &[]);
    assert_eq!(missing.0, 2, "no answer: {missing:?}");

    let unreadable = run_is_affected_with(
        &binary,
        &root,
        &["src/pages/CheckoutPage.tsx"],
        &["src/components/Button.tsx"],
        &["--diff", "no-such.diff"],
    );
    assert_eq!(unreadable.0, 2, "no answer: {unreadable:?}");
}

/// A package whose entry depends on which bundler reads it: `exports` offers a
/// `react-native` build, and `package.json` a `react-native` field, each of which
/// imports something the other entries do not.
fn setup_bundler_package(root: &Path) {
    let package = root.join("node_modules/dual");
    fs::create_dir_all(&package).unwrap();
    fs::create_dir_all(root.join("src/native")).unwrap();
    fs::write(
        package.join("package.json"),
        r#"{
  "name": "dual",
  "main": "./main.js",
  "react-native": "./native-field.js",
  "exports": {
    "./conditioned": { "react-native": "./native-export.js", "default": "./main.js" },
    ".": { "import": "./esm.js", "require": "./main.js" }
  }
}"#,
    )
    .unwrap();
    fs::write(package.join("main.js"), "export const dual = 1;\n").unwrap();
    fs::write(package.join("esm.js"), "export const dual = 1;\n").unwrap();
    fs::write(
        package.join("native-export.js"),
        "import '../../src/native/exported.ts';\nexport const dual = 2;\n",
    )
    .unwrap();
    fs::write(
        package.join("native-field.js"),
        "import '../../src/native/field.ts';\nexport const dual = 3;\n",
    )
    .unwrap();
    fs::write(root.join("src/native/exported.ts"), "export const a = 1;\n").unwrap();
    fs::write(root.join("src/native/field.ts"), "export const b = 1;\n").unwrap();
}

#[test]
fn test_resolve_conditions_and_main_fields_come_from_fallout_toml() {
    let binary = build_binary();
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
        &binary,
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
        &binary,
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
        &binary,
        &root,
        &["src/pages/FieldPage.tsx"],
        &["src/native/field.ts"],
    );
    assert_eq!(code, 0, "the react-native field: {stdout}");
}

#[test]
fn test_a_package_exporting_only_import_and_require_resolves_by_default() {
    let binary = build_binary();
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
        &binary,
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
fn test_each_app_is_answered_with_its_own_bundler() {
    let binary = build_binary();
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
        &binary,
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
        &binary,
        &root,
        &["apps/web/Page.tsx", "apps/mobile/Page.tsx"],
        &["src/native/field.ts"],
    );
    assert_eq!(code, 0, "{stdout}");
}

#[test]
fn test_json_answers_each_anchor_and_classes_what_it_could_not_place() {
    let binary = build_binary();
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
        &binary,
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
fn test_json_classes_a_specifier_by_its_most_in_repo_writer() {
    let binary = build_binary();
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
        &binary,
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
fn test_json_lists_only_what_an_anchors_own_bundler_could_not_place() {
    let binary = build_binary();
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
        &binary,
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
fn test_json_classes_stylesheet_names_and_workspace_packages() {
    let binary = build_binary();
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
        &binary,
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
fn test_json_does_not_take_flags_it_already_answers() {
    let binary = build_binary();
    let temp = TempDir::new().unwrap();
    let root = temp.path().to_path_buf();
    setup_test_project(&root);
    for flag in ["--explain", "--unresolved"] {
        let (code, _, stderr) = run_is_affected_with(
            &binary,
            &root,
            &["src/pages/CheckoutPage.tsx"],
            &["src/components/Button.tsx"],
            &["--json", flag],
        );
        assert_eq!(code, 2, "{flag}: {stderr}");
    }
}
