//! Imports a change may have pointed somewhere else.
//!
//! An import names a file through the rules that resolve it, and a change can move
//! the file without touching the import. A `tsconfig.json` whose `paths` stop
//! mapping `@reduxjs/toolkit` to an app's own shim, or the shim deleted from under
//! the mapping, sends the same specifier to the package, and the file that writes it
//! is as changed as if its import had been rewritten. Nothing in the graph of the
//! current version says so: it only knows where each import goes now.
//!
//! So a run keeps what could have moved an import, and a search asks about each
//! import it meets, which is the only way to reach every importer without reading
//! the whole repository first:
//!
//! - a deleted file, which moved every import that could have named it;
//! - a changed configuration that a tsconfig reads, which moved the imports of the
//!   files it governs as far as the change reaches.
//!
//! How far a tsconfig change reaches is read from the fields module resolution
//! uses. With a base revision both versions are there to compare, and a change to
//! one `paths` entry moves only the specifiers that entry maps. Without one there is
//! no earlier version to compare with, and the change moves every import of every
//! file the tsconfig governs.

use std::path::{Component, Path, PathBuf};

use oxc_resolver::TsConfig;

use crate::base::Base;
use crate::diff::{ChangeSet, FileChange};

/// What a change could have moved, as a run asks about it.
#[derive(Debug, Default)]
pub struct Repointing {
    deleted: Vec<PathBuf>,
    configs: Vec<(PathBuf, Scope)>,
}

/// Which imports of the files a changed tsconfig governs the change can move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Any of them. Which config governs a file, what it extends, or `rootDirs`,
    /// which relative specifiers go through as well, may have changed.
    Everything,
    /// Every specifier that is not relative: `baseUrl` may have changed.
    Mapped,
    /// The specifiers these `paths` entries match, as either version writes them.
    Keys(Vec<String>),
}

impl Repointing {
    pub fn is_empty(&self) -> bool {
        self.deleted.is_empty() && self.configs.is_empty()
    }

    /// Whether a specifier that maps to `candidate` could have named a deleted file.
    ///
    /// A candidate is what the specifier says before any extension or index file is
    /// tried, so a deleted `shims/toolkit.ts` is named by `shims/toolkit`, by
    /// `shims/toolkit.js`, which TypeScript spells it as, and by the file itself.
    pub fn could_name_deleted(&self, candidate: &Path) -> bool {
        let candidate = normalize(candidate);
        self.deleted.iter().any(|deleted| {
            let stem = without_extension(deleted);
            deleted == &candidate
                || stem == candidate
                || stem == without_extension(&candidate)
                || (deleted.file_stem().is_some_and(|name| name == "index")
                    && deleted.parent() == Some(candidate.as_path()))
        })
    }

    pub fn has_deleted(&self) -> bool {
        !self.deleted.is_empty()
    }

    /// Every changed configuration, with how far its change reaches.
    pub fn configs(&self) -> &[(PathBuf, Scope)] {
        &self.configs
    }
}

impl Scope {
    /// Whether the change can move `specifier`.
    pub fn covers(&self, specifier: &str) -> bool {
        match self {
            Scope::Everything => true,
            Scope::Mapped => !is_relative(specifier),
            Scope::Keys(keys) => {
                !is_relative(specifier) && keys.iter().any(|key| key_matches(key, specifier))
            }
        }
    }
}

/// What a change set, and the paths named as changed beside it, could have moved,
/// read once per run.
///
/// Every changed JSON file is kept as a configuration, except the package manager's:
/// whether a tsconfig reads it is a question for each importer, since `extends` can
/// name any file. A named path that is not there is taken as deleted.
pub fn repointing(
    root: &Path,
    changes: &ChangeSet,
    explicit: &[PathBuf],
    base: Option<&Base>,
) -> Repointing {
    let named = changes
        .files
        .iter()
        .map(|file| (root.join(&file.path), file.change == FileChange::Deleted))
        .chain(explicit.iter().map(|path| {
            let path = std::path::absolute(path).unwrap_or_else(|_| path.clone());
            let deleted = !path.exists();
            (path, deleted)
        }));
    let mut repointing = Repointing::default();
    for (path, deleted) in named {
        let path = normalize(&path);
        let path = dunce::canonicalize(&path).unwrap_or(path);
        if deleted && !repointing.deleted.contains(&path) {
            repointing.deleted.push(path.clone());
        }
        if !is_config(&path) || repointing.configs.iter().any(|(known, _)| known == &path) {
            continue;
        }
        if let Some(scope) = scope(&path, deleted, base) {
            repointing.configs.push((path, scope));
        }
    }
    repointing
}

fn is_config(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "json")
        && !path
            .file_name()
            .is_some_and(|name| name == "package.json" || name == "package-lock.json")
}

/// How far a change to the configuration at `path` reaches, or `None` when it
/// reaches no import at all.
fn scope(path: &Path, deleted: bool, base: Option<&Base>) -> Option<Scope> {
    let Some(base) = base else {
        return Some(Scope::Everything);
    };
    let read = |text: Option<String>| match text {
        Some(text) => TsConfig::parse(true, path, path, text).ok(),
        // A version that is not there reads as a config that says nothing.
        None => Some(TsConfig::default()),
    };
    let current = if deleted {
        None
    } else {
        Some(std::fs::read_to_string(path).ok()?)
    };
    let (Some(old), Some(new)) = (read(base.text(path)), read(current)) else {
        // A version that does not parse as a tsconfig may still be read as one.
        return Some(Scope::Everything);
    };
    compare(&old, &new)
}

/// How far the difference between two versions of a tsconfig reaches.
pub fn compare(old: &TsConfig, new: &TsConfig) -> Option<Scope> {
    let (was, is) = (&old.compiler_options, &new.compiler_options);
    let owning = |config: &TsConfig| {
        format!(
            "{:?}",
            (
                &config.extends,
                &config.references,
                &config.files,
                &config.include,
                &config.exclude,
                &config.compiler_options.root_dirs,
            )
        )
    };
    if owning(old) != owning(new) {
        return Some(Scope::Everything);
    }
    if was.base_url != is.base_url {
        return Some(Scope::Mapped);
    }
    let empty = Default::default();
    let (was, is) = (
        was.paths.as_ref().unwrap_or(&empty),
        is.paths.as_ref().unwrap_or(&empty),
    );
    let keys: Vec<String> = was
        .keys()
        .chain(is.keys())
        .filter(|key| was.get(*key) != is.get(*key))
        .cloned()
        .collect();
    (!keys.is_empty()).then_some(Scope::Keys(keys))
}

/// Whether a `paths` key matches a specifier: exactly, or around its one `*`.
fn key_matches(key: &str, specifier: &str) -> bool {
    match key.split_once('*') {
        Some((prefix, suffix)) => {
            specifier.len() >= prefix.len() + suffix.len()
                && specifier.starts_with(prefix)
                && specifier.ends_with(suffix)
        }
        None => key == specifier,
    }
}

pub fn is_relative(specifier: &str) -> bool {
    specifier == "."
        || specifier == ".."
        || specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier.starts_with('/')
}

/// `path` without the extension a module specifier may leave off.
fn without_extension(path: &Path) -> PathBuf {
    const EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "json"];
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) if EXTENSIONS.contains(&extension) => path.with_extension(""),
        _ => path.to_path_buf(),
    }
}

/// Resolves `.` and `..` without asking the file system, which a deleted file is
/// no longer on.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: &str) -> TsConfig {
        let path = Path::new("/repo/tsconfig.json");
        TsConfig::parse(true, path, path, json.to_string()).unwrap()
    }

    #[test]
    fn a_paths_entry_reaches_the_specifiers_it_maps() {
        let old = config(
            r#"{ "compilerOptions": { "paths": { "@reduxjs/toolkit": ["./shim.ts"], "@/*": ["./src/*"] } } }"#,
        );
        let new = config(r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#);
        let scope = compare(&old, &new).unwrap();
        assert_eq!(scope, Scope::Keys(vec!["@reduxjs/toolkit".to_string()]));
        assert!(scope.covers("@reduxjs/toolkit"));
        assert!(!scope.covers("@/components/badge"));
        assert!(!scope.covers("./toolkit"));
    }

    #[test]
    fn what_resolution_does_not_read_reaches_nothing() {
        let old = config(
            r#"{ "compilerOptions": { "strict": false, "paths": { "@/*": ["./src/*"] } } }"#,
        );
        let new =
            config(r#"{ "compilerOptions": { "strict": true, "paths": { "@/*": ["./src/*"] } } }"#);
        assert_eq!(compare(&old, &new), None);
    }

    #[test]
    fn what_decides_which_config_applies_reaches_everything() {
        let old = config(r#"{ "extends": "./base.json" }"#);
        let new = config(r#"{ "extends": "./other.json" }"#);
        assert_eq!(compare(&old, &new), Some(Scope::Everything));
        let old = config(r#"{ "compilerOptions": { "baseUrl": "." } }"#);
        let new = config(r#"{ "compilerOptions": { "baseUrl": "./src" } }"#);
        assert_eq!(compare(&old, &new), Some(Scope::Mapped));
    }

    #[test]
    fn a_key_matches_exactly_or_around_its_star() {
        assert!(key_matches("@/*", "@/a/b"));
        assert!(key_matches("*.css", "theme.css"));
        assert!(!key_matches("@/*", "@acme/ui"));
        assert!(key_matches("lodash", "lodash"));
        assert!(!key_matches("lodash", "lodash/fp"));
    }

    #[test]
    fn a_deleted_file_is_named_with_or_without_its_extension() {
        let repointing = Repointing {
            deleted: vec![
                PathBuf::from("/repo/src/shims/toolkit.ts"),
                PathBuf::from("/repo/src/theme/index.ts"),
            ],
            configs: Vec::new(),
        };
        for named in [
            "/repo/src/shims/toolkit.ts",
            "/repo/src/shims/toolkit",
            "/repo/src/shims/toolkit.js",
            "/repo/src/shims/../shims/toolkit",
            "/repo/src/theme",
        ] {
            assert!(repointing.could_name_deleted(Path::new(named)), "{named}");
        }
        assert!(!repointing.could_name_deleted(Path::new("/repo/src/shims/other")));
    }
}
