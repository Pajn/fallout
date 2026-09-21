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
//! way, and everything else comes from the project's own file. The claim applies to
//! the files below the file that wrote it — see [`crate::config`] for why.

use std::fmt;

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

/// The name of the file a project puts its own entries in.
///
/// The same file carries everything else the project declares. See [`crate::config`],
/// which owns reading it and decides which files each entry speaks for.
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

    /// The built-in entries, if asked for, plus `entries`.
    pub fn of(builtin: bool, entries: Vec<PureCall>) -> Self {
        let mut list = if builtin {
            Self::builtin()
        } else {
            Self::default()
        };
        for entry in entries {
            if !list.entries.contains(&entry) {
                list.entries.push(entry);
            }
        }
        list
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

/// The `pure` list and the `builtin-pure` flag of one already-parsed `fallout.toml`.
///
/// `None` for the flag means the file did not say, which is what lets the nearest
/// file that *did* say decide for a whole subtree.
pub(crate) fn read(
    document: &toml::Table,
    shown: &str,
) -> Result<(Option<bool>, Vec<PureCall>), Error> {
    let builtin = match document.get("builtin-pure") {
        None => None,
        Some(value) => Some(value.as_bool().ok_or_else(|| Error::NotAList {
            path: shown.to_string(),
        })?),
    };

    let Some(pure) = document.get("pure") else {
        return Ok((builtin, Vec::new()));
    };
    let Some(pure) = pure.as_array() else {
        return Err(Error::NotAList {
            path: shown.to_string(),
        });
    };

    let mut entries = Vec::new();
    for value in pure {
        let entry = value.as_str().ok_or_else(|| Error::NotAList {
            path: shown.to_string(),
        })?;
        let parsed = parse_entry(entry).ok_or_else(|| Error::BadEntry {
            path: shown.to_string(),
            entry: entry.to_string(),
        })?;
        if !entries.contains(&parsed) {
            entries.push(parsed);
        }
    }
    Ok((builtin, entries))
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

    fn written(body: &str) -> Result<(Option<bool>, Vec<PureCall>), Error> {
        read(
            &body.parse::<toml::Table>().expect("readable toml"),
            "fallout.toml",
        )
    }

    #[test]
    fn a_project_file_adds_to_the_builtin_list() {
        let (builtin, entries) = written("pure = [\"react-native#StyleSheet.create\"]\n").unwrap();
        let list = PureList::of(builtin.unwrap_or(true), entries);
        assert!(list.contains("react-native", &["StyleSheet", "create"]));
        assert!(list.contains("react", &["memo"]));
    }

    #[test]
    fn a_project_file_can_drop_the_builtin_list() {
        let (builtin, entries) =
            written("builtin-pure = false\npure = [\"./local#make\"]\n").unwrap();
        assert_eq!(builtin, Some(false));
        let list = PureList::of(builtin.unwrap_or(true), entries);
        assert!(list.contains("./local", &["make"]));
        assert!(!list.contains("react", &["memo"]));
    }

    #[test]
    fn a_file_that_says_nothing_leaves_the_builtin_list() {
        let (builtin, entries) = written("").unwrap();
        assert_eq!(builtin, None, "so a nearer file can decide");
        assert!(PureList::of(builtin.unwrap_or(true), entries).contains("react", &["memo"]));
    }

    #[test]
    fn a_misspelt_entry_is_reported_rather_than_ignored() {
        let error = written("pure = [\"memo\"]\n").unwrap_err();
        assert!(matches!(error, Error::BadEntry { .. }), "{error}");
    }
}
