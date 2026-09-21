//! Calls that only compute a value.
//!
//! A declaration whose initialiser runs something belongs to module initialisation,
//! so importing anything from its file reaches it. In React-shaped code nearly every
//! top-level declaration is a call — `memo`, `forwardRef`, `createContext`,
//! `styled`, a stylesheet factory, a `graphql` tag — and none of them do anything a
//! later import could observe. Naming them here takes those declarations back out of
//! initialisation.
//!
//! This is a claim about someone else's function, so it is the project's to make:
//! the built-in list holds only the React factories every bundler already treats this
//! way, and everything else comes from the project's own file.

use std::fmt;
use std::path::Path;

/// A callee declared free of side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PureCall {
    /// The module specifier, written exactly as an import writes it.
    pub source: String,
    /// The path from the imported binding to the callee: `["memo"]`,
    /// `["default", "memo"]`, `["StyleSheet", "create"]`. The first segment is the
    /// name the target exports, with `default` and `*` for the two unnamed forms.
    pub path: Vec<String>,
}

/// Every callee this run treats as pure.
#[derive(Debug, Clone, Default)]
pub struct PureList {
    entries: Vec<PureCall>,
}

/// The name of the file a project puts its own entries in, read from `--root`.
///
/// The same file carries everything else the project declares. See [`crate::config`].
pub use crate::config::FILE as CONFIG_FILE;

/// Factories from React itself. Each returns a value and does nothing else, which is
/// why every bundler drops an unused one.
const BUILTIN: &[&str] = &[
    "react#memo",
    "react#forwardRef",
    "react#createContext",
    "react#lazy",
    "react#default.memo",
    "react#default.forwardRef",
    "react#default.createContext",
    "react#default.lazy",
    "react#*.memo",
    "react#*.forwardRef",
    "react#*.createContext",
    "react#*.lazy",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The file is there but is not readable as TOML.
    Unreadable { path: String, detail: String },
    /// `pure` is present but is not a list of strings.
    NotAList { path: String },
    /// An entry does not name both a source and a callee.
    BadEntry { path: String, entry: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unreadable { path, detail } => write!(f, "{path}: {detail}"),
            Error::NotAList { path } => {
                write!(f, "{path}: `pure` must be a list of strings")
            }
            Error::BadEntry { path, entry } => write!(
                f,
                "{path}: `{entry}` is not a pure call. Write it as \
                 <import source>#<callee>, for example \"react#memo\" or \
                 \"react-native#StyleSheet.create\""
            ),
        }
    }
}

impl PureList {
    /// The built-in entries alone.
    pub fn builtin() -> Self {
        Self {
            entries: BUILTIN.iter().filter_map(|e| parse_entry(e)).collect(),
        }
    }

    /// The built-in entries plus whatever `root/fallout.toml` adds.
    ///
    /// A missing file is not an error: most projects need no entries at all.
    pub fn load(root: &Path) -> Result<Self, Error> {
        let path = root.join(CONFIG_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(Self::builtin());
        };
        let shown = path.display().to_string();

        let document: toml::Table = text.parse().map_err(|error| Error::Unreadable {
            path: shown.clone(),
            detail: format!("{error}"),
        })?;

        let mut list = if document
            .get("builtin-pure")
            .and_then(toml::Value::as_bool)
            .unwrap_or(true)
        {
            Self::builtin()
        } else {
            Self::default()
        };

        let Some(pure) = document.get("pure") else {
            return Ok(list);
        };
        let Some(pure) = pure.as_array() else {
            return Err(Error::NotAList { path: shown });
        };

        for value in pure {
            let entry = value.as_str().ok_or_else(|| Error::NotAList {
                path: shown.clone(),
            })?;
            let parsed = parse_entry(entry).ok_or_else(|| Error::BadEntry {
                path: shown.clone(),
                entry: entry.to_string(),
            })?;
            if !list.entries.contains(&parsed) {
                list.entries.push(parsed);
            }
        }
        Ok(list)
    }

    /// Is a callee reached by `path` from an import of `source` pure?
    pub fn contains(&self, source: &str, path: &[&str]) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.source == source && entry.path.iter().eq(path.iter()))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// `"react-native#StyleSheet.create"` into its two halves.
fn parse_entry(entry: &str) -> Option<PureCall> {
    let (source, callee) = entry.split_once('#')?;
    if source.is_empty() || callee.is_empty() {
        return None;
    }
    let path: Vec<String> = callee.split('.').map(str::to_string).collect();
    if path.iter().any(String::is_empty) {
        return None;
    }
    Some(PureCall {
        source: source.to_string(),
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_splits_into_a_source_and_a_path() {
        assert_eq!(
            parse_entry("react#memo"),
            Some(PureCall {
                source: "react".to_string(),
                path: vec!["memo".to_string()],
            })
        );
        assert_eq!(
            parse_entry("react-native#StyleSheet.create"),
            Some(PureCall {
                source: "react-native".to_string(),
                path: vec!["StyleSheet".to_string(), "create".to_string()],
            })
        );
    }

    #[test]
    fn a_half_written_entry_is_rejected() {
        assert_eq!(parse_entry("react"), None);
        assert_eq!(parse_entry("#memo"), None);
        assert_eq!(parse_entry("react#"), None);
        assert_eq!(parse_entry("react#memo."), None);
        assert_eq!(parse_entry("react#.memo"), None);
    }

    #[test]
    fn the_builtin_list_covers_the_react_factories() {
        let list = PureList::builtin();
        assert!(list.contains("react", &["memo"]));
        assert!(list.contains("react", &["default", "createContext"]));
        assert!(!list.contains("react", &["useState"]));
        // The source has to match: a local `memo` is not React's.
        assert!(!list.contains("./memo", &["memo"]));
    }

    #[test]
    fn a_project_file_adds_to_the_builtin_list() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "pure = [\"react-native#StyleSheet.create\"]\n",
        )
        .unwrap();

        let list = PureList::load(dir.path()).unwrap();
        assert!(list.contains("react-native", &["StyleSheet", "create"]));
        assert!(list.contains("react", &["memo"]));
    }

    #[test]
    fn a_project_file_can_drop_the_builtin_list() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "builtin-pure = false\npure = [\"./local#make\"]\n",
        )
        .unwrap();

        let list = PureList::load(dir.path()).unwrap();
        assert!(list.contains("./local", &["make"]));
        assert!(!list.contains("react", &["memo"]));
    }

    #[test]
    fn a_missing_file_leaves_the_builtin_list() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            PureList::load(dir.path())
                .unwrap()
                .contains("react", &["memo"])
        );
    }

    #[test]
    fn a_misspelt_entry_is_reported_rather_than_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), "pure = [\"memo\"]\n").unwrap();
        let error = PureList::load(dir.path()).unwrap_err();
        assert!(matches!(error, Error::BadEntry { .. }), "{error}");
    }
}
