//! Which packages a change to a lockfile touched.
//!
//! A lockfile entry pins one package to one resolved artefact. When its hash
//! changes, the code behind every import of that package changed, even though
//! nothing in the repository did — so an import of it is a dependency on the
//! lockfile, and the usual graph cannot see it.
//!
//! Only the after version is read, as everywhere else: the diff says which lines
//! moved, and the entry those lines sit inside names the package.
//!
//! # What is read, and what is passed over
//!
//! Only a package's own record is read: its entry in `packages`, which carries
//! the version in its key and the hash in its body, and its entry in
//! `patchedDependencies`, which carries the hash of a patch applied on top. For
//! npm, the `node_modules/` keys, which carry the same two things.
//!
//! Everything else in a lockfile is about relationships rather than about a
//! package. `importers`, `catalogs` and `overrides` record ranges that were
//! asked for, and npm's root entry does the same; a range that moves without
//! moving a resolution installs the same bytes. `snapshots` records which
//! version of each dependency a package resolved to — which changes when a
//! dependency moves, while the package itself does not.
//!
//! Passing `snapshots` over is the point rather than a shortcut. A version is a
//! package's promise about its public API and the hash is the evidence behind
//! the promise, so a package whose version and hash are what they were is the
//! package it was. Naming it because something underneath it moved would report
//! a change to an API that did not change.
//!
//! A file whose format is recognised but in which no entry can be found at all is
//! read as having changed everything, since a parser that understood nothing is
//! not evidence that nothing changed.

use std::path::{Path, PathBuf};

use ahash::AHashSet;

use crate::diff::{ChangeSet, FileChange};

/// The packages a run should treat as changed.
#[derive(Debug, Default)]
pub struct Changed {
    names: AHashSet<String>,
    /// A lockfile changed in a way that named no package in particular.
    everything: bool,
}

impl Changed {
    pub fn is_empty(&self) -> bool {
        !self.everything && self.names.is_empty()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.everything || self.names.contains(name)
    }

    /// Every package named, for reporting. Empty when everything changed, which
    /// is a different statement than nothing having changed.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.names.iter().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    pub fn is_everything(&self) -> bool {
        self.everything
    }

    /// Whether `path` is the node standing for a package this run treats as
    /// changed.
    ///
    /// The node is exactly `node_modules/<name>`. An installed file of the same
    /// package has path left over after the name, so a real file and the node
    /// standing for its package never collide — which is what makes it safe to
    /// answer yes to every name when a lockfile changed in a way that named
    /// none of them.
    pub fn marks(&self, root: &Path, path: &Path) -> bool {
        let Ok(rest) = path.strip_prefix(root.join("node_modules")) else {
            return false;
        };
        let parts: Vec<&str> = rest
            .components()
            .filter_map(|part| part.as_os_str().to_str())
            .collect();
        let name = match parts.as_slice() {
            [scope, package] if scope.starts_with('@') => format!("{scope}/{package}"),
            [package] => (*package).to_string(),
            _ => return false,
        };
        self.contains(&name)
    }
}

/// Reads every lockfile the change names.
///
/// A path given outright carries no line information, so it is read the way a
/// binary diff is: every entry the file holds. That keeps the coarser way of
/// naming a change at least as loud as the finer one, which is what the whole
/// analysis rests on.
pub fn changed(root: &Path, diff: &ChangeSet, explicit: &[PathBuf]) -> Changed {
    let mut changed = Changed::default();

    for path in explicit {
        let Some(format) = format_of(path) else {
            continue;
        };
        let text = std::fs::read_to_string(path)
            .or_else(|_| std::fs::read_to_string(root.join(path)))
            .unwrap_or_default();
        let entries = entries(&text, format);
        if entries.is_empty() {
            changed.everything = true;
            continue;
        }
        changed
            .names
            .extend(entries.into_iter().map(|entry| entry.name));
    }

    for file in &diff.files {
        if file.change == FileChange::Deleted {
            continue;
        }
        let Some(format) = format_of(&file.path) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(root.join(&file.path)) else {
            continue;
        };

        let entries = entries(&text, format);
        if entries.is_empty() {
            changed.everything = true;
            continue;
        }

        match &file.change {
            // No line information, so every entry the file holds is suspect.
            FileChange::Opaque | FileChange::Deleted => {
                changed
                    .names
                    .extend(entries.into_iter().map(|entry| entry.name));
            }
            FileChange::Modified { ranges } => {
                for entry in entries {
                    let touched = ranges.iter().any(|range| {
                        // A pure deletion is a zero-width position; it counts
                        // against the entry it sits inside.
                        let last = range.start + range.len.max(1) - 1;
                        range.start <= entry.end && last >= entry.start
                    });
                    if touched {
                        changed.names.insert(entry.name);
                    }
                }
            }
        }
    }

    changed
}

/// The package a bare import specifier names: `lodash/fp` is `lodash`.
///
/// `None` for anything that is not a package: relative and absolute paths, and
/// the `#name` subpath imports a package declares about itself.
pub fn package_of(specifier: &str) -> Option<&str> {
    if specifier.is_empty()
        || specifier.starts_with('.')
        || specifier.starts_with('/')
        || specifier.starts_with('#')
    {
        return None;
    }

    let mut parts = specifier.split('/');
    let first = parts.next()?;
    if first.starts_with('@') {
        let second = parts.next()?;
        let end = first.len() + 1 + second.len();
        return specifier.get(..end);
    }
    Some(first)
}

/// Where a package is spoken of in the graph.
///
/// The conventional install location, whether or not anything is installed
/// there. A run that decides from the lockfile alone must not need the packages
/// on disk, and this node stands for the package rather than for a file of it.
pub fn node_path(root: &Path, name: &str) -> PathBuf {
    root.join("node_modules").join(name)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Format {
    Pnpm,
    Npm,
}

fn format_of(path: &Path) -> Option<Format> {
    match path.file_name()?.to_str()? {
        "pnpm-lock.yaml" => Some(Format::Pnpm),
        "package-lock.json" => Some(Format::Npm),
        _ => None,
    }
}

/// One entry's line span in the after version, and the package it names.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    start: u32,
    end: u32,
    name: String,
}

fn entries(text: &str, format: Format) -> Vec<Entry> {
    match format {
        Format::Pnpm => pnpm_entries(text),
        Format::Npm => npm_entries(text),
    }
}

/// pnpm writes one entry per package at two spaces of indent, under a top-level
/// section. The file may hold several YAML documents; pnpm keeps its own
/// dependencies in one of its own.
fn pnpm_entries(text: &str) -> Vec<Entry> {
    let mut found = Vec::new();
    let mut open: Option<Entry> = None;
    let mut resolved_section = false;

    for (index, line) in text.lines().enumerate() {
        let number = index as u32 + 1;

        if line == "---" {
            close(&mut open, &mut found, number);
            resolved_section = false;
            continue;
        }

        if let Some(section) = top_level_key(line) {
            close(&mut open, &mut found, number);
            resolved_section = matches!(section, "packages" | "patchedDependencies");
            continue;
        }

        if !resolved_section {
            continue;
        }

        if let Some(key) = keyed_at(line, 2) {
            close(&mut open, &mut found, number);
            if let Some(name) = pnpm_name(key) {
                open = Some(Entry {
                    start: number,
                    end: number,
                    name,
                });
            }
        }
    }

    close(&mut open, &mut found, text.lines().count() as u32 + 1);
    found
}

/// npm keys each entry by where it installs, so the key carries the name. Only
/// the lockfile's own `node_modules/` keys are entries; the root `""` entry
/// holds requested ranges.
fn npm_entries(text: &str) -> Vec<Entry> {
    let mut found = Vec::new();
    let mut open: Option<Entry> = None;

    for (index, line) in text.lines().enumerate() {
        let number = index as u32 + 1;
        let indent = indent_of(line);

        let closes = open
            .as_ref()
            .is_some_and(|entry| !line.trim().is_empty() && indent <= 4 && number > entry.start);
        if closes {
            close(&mut open, &mut found, number);
        }

        if let Some(key) = json_key(line)
            && let Some(name) = npm_name(key)
        {
            open = Some(Entry {
                start: number,
                end: number,
                name,
            });
        }
    }

    close(&mut open, &mut found, text.lines().count() as u32 + 1);
    found
}

fn close(open: &mut Option<Entry>, found: &mut Vec<Entry>, before: u32) {
    if let Some(mut entry) = open.take() {
        entry.end = before.saturating_sub(1).max(entry.start);
        found.push(entry);
    }
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// A top-level YAML key, which is what starts a section.
fn top_level_key(line: &str) -> Option<&str> {
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    let key = match line.strip_suffix(':') {
        Some(key) => key,
        None => line.split_once(':')?.0,
    };
    key.chars()
        .all(|c| c.is_ascii_alphanumeric())
        .then_some(key)
}

/// A mapping key at exactly `indent` spaces.
///
/// The value may follow on the same line: pnpm writes a package with no
/// dependencies as `name@version: {}`, and a patch puts its hash in the key of
/// exactly such an entry, so a reader that only accepted a bare colon would
/// miss the one line that says the bytes moved.
fn keyed_at(line: &str, indent: usize) -> Option<&str> {
    if indent_of(line) != indent {
        return None;
    }
    let rest = line.trim_start();
    let (key, after) = match rest.strip_prefix('\'') {
        Some(quoted) => {
            let end = quoted.find('\'')?;
            (&quoted[..end], &quoted[end + 1..])
        }
        None => {
            let end = rest.find(':')?;
            (&rest[..end], &rest[end..])
        }
    };
    // The colon has to end the key rather than sit inside it.
    let after = after.strip_prefix(':')?;
    (!key.is_empty() && (after.is_empty() || after.starts_with(' '))).then_some(key)
}

/// A JSON object key: `"node_modules/react": {`.
fn json_key(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let rest = line.strip_prefix('"')?;
    let end = rest.find('"')?;
    let after = rest[end + 1..].trim_start();
    after.starts_with(':').then(|| &rest[..end])
}

/// The package a pnpm key names, dropping the version and anything after it:
/// `'@scope/name@1.0.0(peer@2.0.0)'` is `@scope/name`.
///
/// No key that is read carries a suffix today — those live in `snapshots`,
/// which is not read — but a key is a key and this is what one means.
fn pnpm_name(key: &str) -> Option<String> {
    let key = key.trim().trim_matches('\'').trim_matches('"');
    let key = key.split('(').next()?;
    let name = match key.rfind('@').filter(|&at| at > 0) {
        Some(at) => &key[..at],
        None => key,
    };
    (!name.is_empty()).then(|| name.to_string())
}

/// The package an npm key names: `node_modules/a/node_modules/b` is `b`.
fn npm_name(key: &str) -> Option<String> {
    let (_, name) = key.rsplit_once("node_modules/")?;
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{ChangedFile, LineRange};

    const PNPM: &str = "\
lockfileVersion: '9.0'

importers:

  apps/web:
    dependencies:
      react:
        specifier: ^18.0.0
        version: 18.3.1

packages:

  react@18.3.1:
    resolution: {integrity: sha512-aaa==}

  '@scope/widget@2.0.0':
    resolution: {integrity: sha512-bbb==}

snapshots:

  react@18.3.1: {}

  '@scope/widget@2.0.0(react@18.3.1)':
    dependencies:
      react: 18.3.1
";

    const NPM: &str = r#"{
  "name": "app",
  "lockfileVersion": 3,
  "packages": {
    "": {
      "dependencies": {
        "react": "^18.0.0"
      }
    },
    "node_modules/react": {
      "version": "18.3.1",
      "integrity": "sha512-aaa=="
    },
    "node_modules/widget/node_modules/tiny": {
      "version": "1.0.0",
      "integrity": "sha512-ccc=="
    }
  }
}
"#;

    /// The 1-based line the first occurrence of `needle` sits on.
    fn line_of(body: &str, needle: &str) -> u32 {
        body.lines()
            .position(|line| line.contains(needle))
            .expect("the sample holds the needle") as u32
            + 1
    }

    fn touching(name: &str, body: &str, lines: &[u32]) -> Changed {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(name), body).unwrap();
        let ranges = lines
            .iter()
            .map(|&start| LineRange { start, len: 1 })
            .collect();
        changed(
            dir.path(),
            &ChangeSet {
                files: vec![ChangedFile {
                    path: PathBuf::from(name),
                    change: FileChange::Modified { ranges },
                }],
            },
            &[],
        )
    }

    #[test]
    fn a_changed_hash_names_the_package_it_belongs_to() {
        let at = line_of(PNPM, "sha512-aaa");
        let changed = touching("pnpm-lock.yaml", PNPM, &[at]);
        assert_eq!(changed.names(), vec!["react"]);
    }

    #[test]
    fn a_scoped_key_keeps_its_scope_and_loses_its_version() {
        let at = line_of(PNPM, "sha512-bbb");
        let changed = touching("pnpm-lock.yaml", PNPM, &[at]);
        assert_eq!(changed.names(), vec!["@scope/widget"]);
    }

    /// A bump of `deep` leaves `middle` at the version and hash it had. pnpm
    /// records under `middle` which `deep` it resolved to, and that line moves —
    /// but `middle` is the same package, so it is not named.
    const DEPTH: &str = "\
lockfileVersion: '9.0'

packages:

  deep@1.0.1:
    resolution: {integrity: sha512-deep==}

  middle@2.0.0:
    resolution: {integrity: sha512-middle==}

snapshots:

  deep@1.0.1: {}

  middle@2.0.0:
    dependencies:
      deep: 1.0.1
";

    #[test]
    fn a_package_whose_dependency_moved_is_not_named() {
        let changed = touching(
            "pnpm-lock.yaml",
            DEPTH,
            &[line_of(DEPTH, "      deep: 1.0.1")],
        );
        assert!(changed.names().is_empty(), "{:?}", changed.names());
    }

    #[test]
    fn the_package_that_moved_is_still_named() {
        let changed = touching("pnpm-lock.yaml", DEPTH, &[line_of(DEPTH, "  deep@1.0.1:")]);
        assert_eq!(changed.names(), vec!["deep"]);
    }

    /// pnpm writes the peer it resolved into the snapshot key, so a package
    /// built against a bumped peer has a snapshot of its own that moved. Its
    /// version and hash did not, so neither did its public API.
    #[test]
    fn a_package_rebuilt_against_a_new_peer_is_not_named() {
        let changed = touching("pnpm-lock.yaml", PNPM, &[line_of(PNPM, "(react@18.3.1)")]);
        assert!(changed.names().is_empty(), "{:?}", changed.names());
    }

    /// A patch changes the bytes without changing the version, which is the one
    /// way a package can move while its `packages` entry stands still. pnpm
    /// records it against the package, and so does this.
    #[test]
    fn a_new_patch_names_the_package_it_patches() {
        const PATCHED: &str = "\
lockfileVersion: '9.0'

patchedDependencies:
  '@vendor/plot@3.0.0': beef
  other@1.0.0: cafe

packages:

  '@vendor/plot@3.0.0':
    resolution: {integrity: sha512-plot==}
";
        let changed = touching("pnpm-lock.yaml", PATCHED, &[line_of(PATCHED, "': beef")]);
        assert_eq!(changed.names(), vec!["@vendor/plot"]);
    }

    /// A key is a key, whatever pnpm decides to hang off one.
    #[test]
    fn a_key_loses_its_version_and_anything_after_it() {
        assert_eq!(pnpm_name("react@18.3.1").as_deref(), Some("react"));
        assert_eq!(
            pnpm_name("'@scope/name@1.0.0(peer@2.0.0)'").as_deref(),
            Some("@scope/name")
        );
        assert_eq!(
            pnpm_name("'@lodev09/react-native-true-sheet'").as_deref(),
            Some("@lodev09/react-native-true-sheet")
        );
    }

    #[test]
    fn the_range_that_was_asked_for_names_nothing() {
        let at = line_of(PNPM, "specifier: ^18.0.0");
        let changed = touching("pnpm-lock.yaml", PNPM, &[at]);
        assert!(changed.names().is_empty(), "{:?}", changed.names());
        assert!(!changed.is_everything());
    }

    #[test]
    fn an_npm_key_names_the_package_it_installs() {
        let at = line_of(NPM, "sha512-aaa");
        let changed = touching("package-lock.json", NPM, &[at]);
        assert_eq!(changed.names(), vec!["react"]);
    }

    #[test]
    fn a_nested_npm_key_names_the_inner_package() {
        let at = line_of(NPM, "sha512-ccc");
        let changed = touching("package-lock.json", NPM, &[at]);
        assert_eq!(changed.names(), vec!["tiny"]);
    }

    #[test]
    fn the_npm_root_entry_names_nothing() {
        let at = line_of(NPM, r#""react": "^18.0.0""#);
        let changed = touching("package-lock.json", NPM, &[at]);
        assert!(changed.names().is_empty(), "{:?}", changed.names());
    }

    #[test]
    fn a_lockfile_holding_no_entry_is_read_as_changing_everything() {
        let changed = touching("pnpm-lock.yaml", "lockfileVersion: '9.0'\n", &[1]);
        assert!(changed.is_everything());
        assert!(changed.contains("anything-at-all"));
    }

    #[test]
    fn a_file_that_is_not_a_lockfile_is_passed_over() {
        let changed = touching("package.json", NPM, &[1]);
        assert!(changed.is_empty());
    }

    #[test]
    fn a_specifier_names_the_package_it_reaches_into() {
        assert_eq!(package_of("lodash"), Some("lodash"));
        assert_eq!(package_of("lodash/fp"), Some("lodash"));
        assert_eq!(package_of("@scope/widget"), Some("@scope/widget"));
        assert_eq!(package_of("@scope/widget/dist/x.js"), Some("@scope/widget"));
        assert_eq!(package_of("./local"), None);
        assert_eq!(package_of("../local"), None);
        assert_eq!(package_of("/absolute"), None);
        assert_eq!(package_of("#app/lib"), None);
    }

    /// The node standing for a package and an installed file of that package
    /// live under the same directory, and only one of them is the package.
    #[test]
    fn a_file_of_a_changed_package_is_not_the_package() {
        let root = Path::new("/repo");
        let mut changed = Changed::default();
        changed.names.insert("react".to_string());
        changed.names.insert("@scope/widget".to_string());

        assert!(changed.marks(root, &node_path(root, "react")));
        assert!(changed.marks(root, &node_path(root, "@scope/widget")));
        assert!(!changed.marks(root, Path::new("/repo/node_modules/react/index.js")));
        assert!(!changed.marks(root, Path::new("/repo/node_modules/@scope/widget/x.js")));
        assert!(!changed.marks(root, Path::new("/repo/src/react")));
    }

    /// Answering yes to every name must not start answering yes to every file.
    #[test]
    fn everything_changing_still_does_not_mark_an_installed_file() {
        let root = Path::new("/repo");
        let changed = Changed {
            names: AHashSet::default(),
            everything: true,
        };
        assert!(changed.marks(root, &node_path(root, "react")));
        assert!(!changed.marks(root, Path::new("/repo/node_modules/react/index.js")));
    }
}
