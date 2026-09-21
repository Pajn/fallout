use std::fs;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

fn build_binary() -> PathBuf {
    let output = Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("Failed to build binary");

    if !output.status.success() {
        panic!(
            "Build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.join("target/release/fallout")
}

fn run_is_affected(
    binary: &PathBuf,
    root: &PathBuf,
    anchors: &[&str],
    changed: &[&str],
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

    let output = cmd.output().expect("Failed to execute is_affected");

    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn setup_test_project(root: &PathBuf) {
    fs::create_dir_all(root.join("src/components")).unwrap();
    fs::create_dir_all(root.join("src/utils")).unwrap();
    fs::create_dir_all(root.join("src/pages")).unwrap();
    fs::create_dir_all(root.join("src/assets")).unwrap();

    // A real PNG header, so the file is not valid UTF-8.
    fs::write(
        root.join("src/assets/logo.png"),
        [0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a],
    ).unwrap();

    fs::write(
        root.join("src/assets/icon.svg"),
        r#"<svg xmlns="http://www.w3.org/2000/svg"><rect /></svg>"#,
    ).unwrap();

    fs::write(
        root.join("src/assets/theme.css"),
        r#".button { color: red; }"#,
    ).unwrap();

    fs::write(
        root.join("src/assets/beep.mp3"),
        [0x49u8, 0x44, 0x33, 0x04],
    ).unwrap();

    fs::write(
        root.join("src/components/Button.tsx"),
        r#"export const Button = () => <button>Click</button>;"#,
    ).unwrap();

    fs::write(
        root.join("src/components/Card.tsx"),
        r#"import { Button } from "./Button";
export const Card = () => <div><Button /></div>;"#,
    ).unwrap();

    fs::write(
        root.join("src/utils/helpers.ts"),
        r#"export const formatDate = (d: Date) => d.toISOString();"#,
    ).unwrap();

    fs::write(
        root.join("src/pages/CheckoutPage.tsx"),
        r#"import { Card } from "../components/Card";
import { formatDate } from "../utils/helpers";
export const CheckoutPage = () => <Card />;"#,
    ).unwrap();

    fs::write(
        root.join("src/pages/SettingsPage.tsx"),
        r#"import { Button } from "../components/Button";
export const SettingsPage = () => <Button />;"#,
    ).unwrap();

    fs::write(
        root.join("src/App.tsx"),
        r#"import { CheckoutPage } from "./pages/CheckoutPage";
import { SettingsPage } from "./pages/SettingsPage";
export const App = () => <> <CheckoutPage /> <SettingsPage /> </>;"#,
    ).unwrap();
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
    assert_eq!(code, 0, "Expected exit code 0 (affected), got {}. stdout: {}", code, stdout);
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
    assert_eq!(code2, 1, "Expected exit code 1 (not affected), got {}. stdout: {}", code2, stdout2);
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

    assert_eq!(code, 0, "Expected exit code 0 (upstream impact), got {}. stdout: {}", code, stdout);
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

    assert_eq!(code, 0, "Expected exit code 0 (impact on at least one anchor), got {}. stdout: {}", code, stdout);
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
    ).unwrap();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
        &root,
        &["src/pages/CheckoutPage.tsx", "src/pages/SettingsPage.tsx"],
        &["src/unrelated.ts"],
    );

    assert_eq!(code, 1, "Expected exit code 1 (no impact), got {}. stdout: {}", code, stdout);
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
    ).unwrap();

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
        &root,
        &["src/pages/LazyPage.tsx"],
        &["src/components/Card.tsx"],
    );

    assert_eq!(code, 0, "Expected exit code 0 (dynamic import detected), got {}. stdout: {}", code, stdout);
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
    ).unwrap();

    fs::write(
        root.join("src/pages/IndexPage.tsx"),
        r#"import { Button, Card } from "../components";
export const IndexPage = () => <> <Button /> <Card /> </>;"#,
    ).unwrap();

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
        &root,
        &["src/pages/IndexPage.tsx"],
        &["src/components/Button.tsx"],
    );

    assert_eq!(code, 0, "Expected exit code 0 (export-from detected), got {}. stdout: {}", code, stdout);
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

    assert_ne!(output.status.code(), Some(0), "Expected non-zero exit code when no anchor provided");
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

    assert_ne!(code, 0, "Expected non-zero exit code for invalid anchor, got {}. stdout: {} stderr: {}", code, stdout, stderr);
    assert!(stderr.contains("Anchor(s) not found"), "Expected error message about missing anchor, got stderr: {}", stderr);
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
    ).unwrap();

    fs::write(
        root.join("src/pages/LogoPage.tsx"),
        r#"import { Logo } from "../components/Logo";
export const LogoPage = () => <Logo />;"#,
    ).unwrap();

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
        &root,
        &["src/pages/LogoPage.tsx"],
        &["src/assets/logo.png"],
    );

    assert_eq!(code, 0, "Expected exit code 0 (imported image affected), got {}. stdout: {}", code, stdout);
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
    ).unwrap();

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
        &root,
        &["src/pages/SoundPage.tsx"],
        &["src/assets/beep.mp3"],
    );

    assert_eq!(code, 0, "Expected exit code 0 (required asset affected), got {}. stdout: {}", code, stdout);
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
    ).unwrap();

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
        &root,
        &["src/pages/QueryPage.tsx"],
        &["src/assets/icon.svg"],
    );

    assert_eq!(code, 0, "Expected exit code 0 (asset behind a resource query), got {}. stdout: {}", code, stdout);
    assert!(stdout.contains("src/assets/icon.svg"));

    let (code2, stdout2, _stderr2) = run_is_affected(
        &binary,
        &root,
        &["src/pages/QueryPage.tsx"],
        &["src/assets/logo.png"],
    );

    assert_eq!(code2, 0, "Expected exit code 0 (png behind ?url), got {}. stdout: {}", code2, stdout2);
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
    ).unwrap();

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
        &root,
        &["src/pages/LoaderPage.tsx"],
        &["src/assets/logo.png"],
    );

    assert_eq!(code, 0, "Expected exit code 0 (webpack inline loader), got {}. stdout: {}", code, stdout);
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
    ).unwrap();

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
        &root,
        &["src/pages/UrlPage.tsx"],
        &["src/assets/beep.mp3"],
    );

    assert_eq!(code, 0, "Expected exit code 0 (new URL asset reference), got {}. stdout: {}", code, stdout);
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
    ).unwrap();

    // CheckoutPage reaches Card/Button, never Logo — so the image is out of its graph.
    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
        &root,
        &["src/pages/CheckoutPage.tsx"],
        &["src/assets/logo.png"],
    );

    assert_eq!(code, 1, "Expected exit code 1 (asset outside the anchor graph), got {}. stdout: {}", code, stdout);
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
    ).unwrap();

    let binary = build_binary();

    let (code, stdout, _stderr) = run_is_affected(
        &binary,
        &root,
        &["src/pages/IconPage.tsx"],
        &["src/components/Button.tsx"],
    );

    assert_eq!(code, 1, "Expected exit code 1 (no path through the asset), got {}. stdout: {}", code, stdout);
    assert!(stdout.contains("No reachability impact detected"));
}
