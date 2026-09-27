//! What the test files share: a sample project to lay down, and a fixture made into
//! the repository a `--base` run reads.
//!
//! Each test file compiles this on its own and uses only some of it.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The fixture of that name, under `tests/fixtures`.
pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Every file under `root`, by path relative to it.
pub fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
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
pub fn repository(fixture: &Path) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let repo = dunce::canonicalize(dir.path()).expect("canonical temp dir");
    copy(&tree(&fixture.join("before")), &repo);
    git(&repo, &["init", "--quiet"]);
    git(&repo, &["add", "--all", "--force"]);
    git(
        &repo,
        &["commit", "--quiet", "--allow-empty", "--message", "before"],
    );
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

/// The fixture's changed files as `git diff --name-only` lists them.
pub fn changed(fixture: &Path) -> Vec<PathBuf> {
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

/// A small app: two pages, the components and helpers they import, and assets of
/// several kinds, one of them not text.
pub fn setup_test_project(root: &Path) {
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

/// Lays a file with a mix of specifiers over the sample project: one that names
/// nothing, and two that name no file by design.
pub fn setup_unresolved_project(root: &Path) {
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

/// A package whose entry depends on which bundler reads it: `exports` offers a
/// `react-native` build, and `package.json` a `react-native` field, each of which
/// imports something the other entries do not.
pub fn setup_bundler_package(root: &Path) {
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

/// Takes read permission away from a file for as long as the guard lives.
#[cfg(unix)]
pub struct Unreadable(PathBuf);

#[cfg(unix)]
impl Unreadable {
    /// `None` where taking the permission away does not stop this process reading
    /// the file, as when it runs as root, which permissions do not bind.
    pub fn make(path: PathBuf) -> Option<Self> {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let guard = Self(path);
        fs::read(&guard.0).is_err().then_some(guard)
    }
}

#[cfg(unix)]
impl Drop for Unreadable {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o644));
    }
}

/// A project whose `src/tsconfig.json` governs `src/page.ts` and maps `@ui/*`
/// through the config it extends. A package named `@ui/button` is installed too,
/// which is where the import goes when the mapping is not read.
pub fn setup_tsconfig_project(root: &Path) {
    let files = [
        (
            "src/tsconfig.json",
            r#"{ "extends": "../tsconfig.base.json" }"#,
        ),
        (
            "tsconfig.base.json",
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@ui/*": ["src/ui/*"] } } }"#,
        ),
        (
            "src/page.ts",
            "import { button } from \"@ui/button\";\nexport const page = button;\n",
        ),
        ("src/ui/button.ts", "export const button = 1;\n"),
        (
            "node_modules/@ui/button/package.json",
            r#"{ "name": "@ui/button", "main": "index.js" }"#,
        ),
        ("node_modules/@ui/button/index.js", "exports.button = 2;\n"),
    ];
    for (path, text) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().expect("a parent")).unwrap();
        fs::write(path, text).unwrap();
    }
}
