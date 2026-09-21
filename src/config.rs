//! The project's `fallout.toml`.
//!
//! Things this tool cannot work out for itself and will not guess at. Each one is a
//! claim the project makes about its own code, in a file the project owns.
//!
//! A malformed entry is an error rather than something skipped. A setting that
//! silently does nothing shows up later as a verdict nobody can explain, and the
//! whole point of declaring it was to be believed.

use std::fmt;
use std::path::Path;

use oxc_resolver::{Alias, AliasValue};

/// The name of the file, read from `--root`.
pub const FILE: &str = "fallout.toml";

/// The `[style]` table: how stylesheets are read and resolved.
#[derive(Debug, Clone, Default)]
pub struct Style {
    /// Names a stylesheet may import that are not paths, and where each points.
    ///
    /// Written without the `~` a bundler spells them with, because the `~` is
    /// dropped before anything is looked up — so one entry covers both spellings.
    pub aliases: Alias,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The file is there but is not readable as TOML.
    Unreadable { path: String, detail: String },
    /// `[style]` is present but is not a table.
    NotATable { path: String, key: String },
    /// An alias points at something that is not a path or a list of them.
    BadAlias { path: String, name: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unreadable { path, detail } => write!(f, "{path}: {detail}"),
            Error::NotATable { path, key } => write!(f, "{path}: `{key}` must be a table"),
            Error::BadAlias { path, name } => write!(
                f,
                "{path}: alias `{name}` must be a path or a list of paths, relative to \
                 this file — for example sass = \"app/sass\""
            ),
        }
    }
}

impl Style {
    /// Reads the `[style]` table of `root/fallout.toml`.
    ///
    /// A missing file is not an error: most projects need no entries at all.
    pub fn load(root: &Path) -> Result<Self, Error> {
        let path = root.join(FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(Self::default());
        };
        let shown = path.display().to_string();

        let document: toml::Table = text.parse().map_err(|error| Error::Unreadable {
            path: shown.clone(),
            detail: format!("{error}"),
        })?;

        let Some(style) = document.get("style") else {
            return Ok(Self::default());
        };
        let style = style.as_table().ok_or_else(|| Error::NotATable {
            path: shown.clone(),
            key: "style".to_string(),
        })?;

        let Some(aliases) = style.get("aliases") else {
            return Ok(Self::default());
        };
        let aliases = aliases.as_table().ok_or_else(|| Error::NotATable {
            path: shown.clone(),
            key: "style.aliases".to_string(),
        })?;

        let mut out = Alias::new();
        for (name, value) in aliases {
            // A target is relative to the file that declares it, and the resolver
            // wants somewhere it can start from rather than another specifier.
            let targets: Vec<AliasValue> = match value {
                toml::Value::String(one) => vec![absolute(root, one)],
                toml::Value::Array(many) => many
                    .iter()
                    .map(|each| {
                        each.as_str()
                            .map(|each| absolute(root, each))
                            .ok_or_else(|| Error::BadAlias {
                                path: shown.clone(),
                                name: name.clone(),
                            })
                    })
                    .collect::<Result<_, _>>()?,
                _ => {
                    return Err(Error::BadAlias {
                        path: shown.clone(),
                        name: name.clone(),
                    });
                }
            };
            out.push((name.clone(), targets));
        }
        Ok(Self { aliases: out })
    }
}

fn absolute(root: &Path, target: &str) -> AliasValue {
    AliasValue::Path(root.join(target).to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(body: &str) -> Result<Style, Error> {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(FILE), body).expect("writing the config");
        Style::load(dir.path())
    }

    fn names(style: &Style) -> Vec<&str> {
        style
            .aliases
            .iter()
            .map(|(name, _)| name.as_str())
            .collect()
    }

    #[test]
    fn no_file_is_no_entries() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(Style::load(dir.path()).expect("no file").aliases.is_empty());
    }

    #[test]
    fn a_file_without_the_table_is_no_entries() {
        let style = written("pure = [\"react#memo\"]\n").expect("readable");
        assert!(style.aliases.is_empty());
    }

    #[test]
    fn an_alias_points_somewhere_below_the_file() {
        let style = written("[style.aliases]\nsass = \"app/sass\"\n").expect("readable");
        assert_eq!(names(&style), vec!["sass"]);
        let AliasValue::Path(target) = &style.aliases[0].1[0] else {
            panic!("a path");
        };
        assert!(
            target.ends_with("app/sass") && target.starts_with('/'),
            "a target is absolute and below the file: {target}"
        );
    }

    #[test]
    fn a_list_is_tried_in_the_order_it_is_written() {
        let style =
            written("[style.aliases]\nsass = [\"a/sass\", \"b/sass\"]\n").expect("readable");
        let targets: Vec<String> = style.aliases[0]
            .1
            .iter()
            .map(|value| match value {
                AliasValue::Path(path) => path.clone(),
                AliasValue::Ignore => "ignore".to_string(),
            })
            .collect();
        assert_eq!(targets.len(), 2);
        assert!(targets[0].ends_with("a/sass"), "{targets:?}");
        assert!(targets[1].ends_with("b/sass"), "{targets:?}");
    }

    #[test]
    fn a_target_that_is_not_a_path_is_an_error() {
        assert!(matches!(
            written("[style.aliases]\nsass = 3\n"),
            Err(Error::BadAlias { .. })
        ));
        assert!(matches!(
            written("[style.aliases]\nsass = [\"ok\", 3]\n"),
            Err(Error::BadAlias { .. })
        ));
    }

    #[test]
    fn a_table_that_is_not_a_table_is_an_error() {
        assert!(matches!(
            written("style = 3\n"),
            Err(Error::NotATable { .. })
        ));
        assert!(matches!(
            written("[style]\naliases = 3\n"),
            Err(Error::NotATable { .. })
        ));
    }

    #[test]
    fn a_file_that_is_not_toml_is_an_error() {
        assert!(matches!(written("[style\n"), Err(Error::Unreadable { .. })));
    }
}
