//! Calls that have no side effects.
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

impl PureList {
    /// The built-in entries alone.
    pub fn builtin() -> Self {
        Self {
            entries: BUILTIN.iter().filter_map(|e| PureCall::parse(e)).collect(),
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

impl PureCall {
    /// `"react-native#StyleSheet.create"` into its two halves, or `None` for an
    /// entry that does not name both.
    pub(crate) fn parse(entry: &str) -> Option<Self> {
        let (source, callee) = entry.split_once('#')?;
        if source.is_empty() || callee.is_empty() {
            return None;
        }
        let path: Vec<String> = callee.split('.').map(str::to_string).collect();
        if path.iter().any(String::is_empty) {
            return None;
        }
        Some(Self {
            source: source.to_string(),
            path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_splits_into_a_source_and_a_path() {
        assert_eq!(
            PureCall::parse("react#memo"),
            Some(PureCall {
                source: "react".to_string(),
                path: vec!["memo".to_string()],
            })
        );
        assert_eq!(
            PureCall::parse("react-native#StyleSheet.create"),
            Some(PureCall {
                source: "react-native".to_string(),
                path: vec!["StyleSheet".to_string(), "create".to_string()],
            })
        );
    }

    #[test]
    fn a_half_written_entry_is_rejected() {
        assert_eq!(PureCall::parse("react"), None);
        assert_eq!(PureCall::parse("#memo"), None);
        assert_eq!(PureCall::parse("react#"), None);
        assert_eq!(PureCall::parse("react#memo."), None);
        assert_eq!(PureCall::parse("react#.memo"), None);
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
}
