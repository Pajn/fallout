//! The project's `fallout.toml`.
//!
//! Things this tool cannot work out for itself and will not guess at. Each one is a
//! claim the project makes about its own code, in a file the project owns.
//!
//! A malformed entry is an error rather than something skipped. A setting that
//! silently does nothing shows up later as a verdict nobody can explain, and the
//! whole point of declaring it was to be believed.
//!
//! # Where a claim applies
//!
//! A monorepo is not one project. Its apps are bundled by different tools and its
//! packages give the same name different meanings, so a single file at the root
//! cannot state what is true of all of them: one `inline-requires` would be a false
//! claim about every app that does not inline, and one `[style.aliases]` table hands
//! every app's names to every other app's directories.
//!
//! So a claim is scoped to where it is written. `d/fallout.toml` speaks for the files
//! under `d`, the chain is walked from a file up to `--root`, and what happens at each
//! step follows from the direction the setting is wrong in:
//!
//! - **Aliases accumulate**, nearest first. A name an app resolves and an ancestor
//!   also resolves gets both, tried in that order. Accumulating rather than
//!   overriding is what keeps the chain from ever offering fewer candidates than the
//!   root alone, and a name that resolves to nothing loses an edge. Within one file
//!   the more specific key is tried first, so `#app/assets/*` beats `#app/*` however
//!   the two were written down.
//! - **`pure` accumulates** the same way, and an entry applies only below the file
//!   that wrote it. A package calling its own factory pure cannot quiet a call in an
//!   app that never made the claim.
//! - **`builtin-pure` and `inline-requires` are single answers**, so the nearest one
//!   wins.
//!
//! # Which file asks
//!
//! Not every claim is asked by the same file, because not every claim is about the
//! same thing.
//!
//! An alias belongs to the stylesheet doing the importing: `packages/ui/card.scss`
//! means one thing by `settings` whoever bundles it. So does a pure call, which is a
//! claim about a function the file calls. Both are read from the chain above *that*
//! file.
//!
//! `inline-requires` is not a property of a file at all. It is a property of the
//! bundler, and the bundler is chosen by the app being asked about — the same shared
//! module is inlined when a mobile bundler pulls it in and is not when a web bundler
//! does. So it is read from the chain above the *anchor*, and with several anchors it
//! holds only if every one of them claims it, since it is the setting that narrows.
//!
//! # When a file is read
//!
//! Lazily, and cached per directory: a run reads the configs it needs for the
//! question it was asked. A malformed file in a subtree the run never enters is never
//! reported, and could not have changed the answer if it had been.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ahash::AHashMap;
use oxc_resolver::{Alias, AliasValue};

use crate::pure::{PureCall, PureList};

/// The name of the file. Read from `--root`, and from any directory beneath it.
pub const FILE: &str = "fallout.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The file is there but is not readable as TOML.
    Unreadable { path: String, detail: String },
    /// `[style]` is present but is not a table.
    NotATable { path: String, key: String },
    /// An alias points at something that is not a path or a list of them.
    BadAlias { path: String, name: String },
    /// A setting that is either on or off was written as something else.
    NotABoolean { path: String, key: String },
    /// Something wrong with this file's `pure` list.
    Pure(crate::pure::Error),
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
            Error::NotABoolean { path, key } => {
                write!(f, "{path}: `{key}` must be true or false")
            }
            Error::Pure(error) => write!(f, "{error}"),
        }
    }
}

impl From<crate::pure::Error> for Error {
    fn from(error: crate::pure::Error) -> Self {
        Error::Pure(error)
    }
}

/// One `fallout.toml`, as written.
#[derive(Debug, Default)]
struct Declared {
    aliases: Alias,
    style_aliases: Alias,
    pure: Vec<PureCall>,
    builtin_pure: Option<bool>,
    inline_requires: Option<bool>,
}

/// What the files above one directory add up to.
///
/// Built once per directory and shared. The directories that contributed are kept
/// because they are the identity of the answer: two files with the same chain get the
/// same aliases, which is what lets a resolver be built once and reused.
#[derive(Debug)]
pub struct Chain {
    dirs: Vec<PathBuf>,
    aliases: Alias,
    style_aliases: Alias,
    pure: PureList,
    inline_requires: bool,
}

impl Chain {
    /// Every directory that contributed, nearest first. Equal lists mean equal
    /// answers, so this is usable as a cache key.
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// Alias names for any specifier, nearest first. See the module docs for why
    /// they accumulate.
    pub fn aliases(&self) -> &Alias {
        &self.aliases
    }

    /// The same for a stylesheet: what `[style.aliases]` declares, and then
    /// everything `[aliases]` declares.
    ///
    /// A stylesheet gets both because a bundler gives it both — one `resolve.alias`
    /// answers every import in the tree. The stylesheet-only table comes first so a
    /// name meaning one thing in Sass and another in JavaScript can say so.
    pub fn style_aliases(&self) -> &Alias {
        &self.style_aliases
    }

    pub fn pure(&self) -> &PureList {
        &self.pure
    }

    pub fn inline_requires(&self) -> bool {
        self.inline_requires
    }
}

/// Every `fallout.toml` at or below `--root`, read on demand.
#[derive(Debug)]
pub struct Configs {
    root: PathBuf,
    /// One directory's own file, or `None` if it has not got one.
    declared: RwLock<AHashMap<PathBuf, Option<Arc<Declared>>>>,
    /// What a directory inherits, once worked out.
    chains: RwLock<AHashMap<PathBuf, Arc<Chain>>>,
    /// The first thing that could not be read.
    ///
    /// Held rather than returned because the answers are wanted deep inside the
    /// resolver and the analyser, where there is nothing useful to do with an error.
    /// The run checks this before reporting a verdict, so a file that cannot be read
    /// replaces the answer rather than quietly shaping it.
    failure: RwLock<Option<Error>>,
}

impl Configs {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            declared: RwLock::new(AHashMap::default()),
            chains: RwLock::new(AHashMap::default()),
            failure: RwLock::new(None),
        }
    }

    /// What `file` inherits, from the directory holding it up to `--root`.
    pub fn chain(&self, file: &Path) -> Arc<Chain> {
        let start = match file.parent() {
            Some(parent) if !file.is_dir() => parent.to_path_buf(),
            _ => file.to_path_buf(),
        };
        if let Some(cached) = self.chains.read().unwrap().get(&start) {
            return cached.clone();
        }

        let mut dirs = Vec::new();
        let mut declared = Vec::new();
        for dir in self.upwards(&start) {
            if let Some(found) = self.declared(&dir) {
                dirs.push(dir);
                declared.push(found);
            }
        }

        let chain = Arc::new(Chain {
            aliases: declared
                .iter()
                .flat_map(|one| one.aliases.iter().cloned())
                .collect(),
            style_aliases: declared
                .iter()
                .flat_map(|one| one.style_aliases.iter().chain(one.aliases.iter()).cloned())
                .collect(),
            pure: PureList::of(
                declared
                    .iter()
                    .find_map(|one| one.builtin_pure)
                    .unwrap_or(true),
                declared
                    .iter()
                    .flat_map(|one| one.pure.iter().cloned())
                    .collect(),
            ),
            inline_requires: declared
                .iter()
                .find_map(|one| one.inline_requires)
                .unwrap_or(false),
            dirs,
        });

        self.chains.write().unwrap().insert(start, chain.clone());
        chain
    }

    /// Whether this run may treat an import as deferred to its first use.
    ///
    /// Read from the anchors rather than from each file, because it describes the
    /// bundler and the anchor is what picks one. Several anchors have to agree: this
    /// is the setting that drops edges, so one anchor in an app that does not inline
    /// is enough to turn it off for the run.
    pub fn inline_requires(&self, anchors: &[PathBuf]) -> bool {
        !anchors.is_empty()
            && anchors
                .iter()
                .all(|anchor| self.chain(anchor).inline_requires())
    }

    /// The first file that could not be read, if any.
    pub fn failure(&self) -> Option<Error> {
        self.failure.read().unwrap().clone()
    }

    /// `start` and each ancestor up to and including `--root`.
    ///
    /// A file outside the root is answered by the root alone: `--root` is the edge of
    /// what this run was pointed at, and reading above it would take claims from a
    /// project nobody named.
    fn upwards(&self, start: &Path) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        let mut here = Some(start);
        while let Some(dir) = here {
            if !dir.starts_with(&self.root) {
                break;
            }
            dirs.push(dir.to_path_buf());
            if dir == self.root {
                return dirs;
            }
            here = dir.parent();
        }
        vec![self.root.clone()]
    }

    fn declared(&self, dir: &Path) -> Option<Arc<Declared>> {
        if let Some(cached) = self.declared.read().unwrap().get(dir) {
            return cached.clone();
        }
        let found = match read(&dir.join(FILE), dir) {
            Ok(found) => found.map(Arc::new),
            Err(error) => {
                self.note(error);
                None
            }
        };
        self.declared
            .write()
            .unwrap()
            .insert(dir.to_path_buf(), found.clone());
        found
    }

    fn note(&self, error: Error) {
        let mut failure = self.failure.write().unwrap();
        if failure.is_none() {
            *failure = Some(error);
        }
    }
}

/// Reads one file. `None` when there is not one here.
fn read(path: &Path, dir: &Path) -> Result<Option<Declared>, Error> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let shown = path.display().to_string();

    let document: toml::Table = text.parse().map_err(|error| Error::Unreadable {
        path: shown.clone(),
        detail: format!("{error}"),
    })?;

    let (builtin_pure, pure) = crate::pure::read(&document, &shown)?;
    Ok(Some(Declared {
        aliases: table(document.get("aliases"), dir, &shown, "aliases")?,
        style_aliases: style_aliases(&document, dir, &shown)?,
        pure,
        builtin_pure,
        inline_requires: flag(&document, "inline-requires", &shown)?,
    }))
}

/// The `[style.aliases]` table, with each target made absolute.
fn style_aliases(document: &toml::Table, dir: &Path, shown: &str) -> Result<Alias, Error> {
    let Some(style) = document.get("style") else {
        return Ok(Alias::new());
    };
    let style = style.as_table().ok_or_else(|| Error::NotATable {
        path: shown.to_string(),
        key: "style".to_string(),
    })?;
    table(style.get("aliases"), dir, shown, "style.aliases")
}

/// One table of names, with each target made absolute and the whole sorted so the
/// most specific key is tried first.
///
/// The resolver takes the first key that matches, and a table is read in whatever
/// order the file format hands it over — which for TOML is alphabetical, and
/// alphabetical puts `#app/*` before `#app/assets/*`. Nobody writing the two would
/// mean the wildcard to win, so specificity decides rather than spelling.
fn table(value: Option<&toml::Value>, dir: &Path, shown: &str, key: &str) -> Result<Alias, Error> {
    let Some(aliases) = value else {
        return Ok(Alias::new());
    };
    let aliases = aliases.as_table().ok_or_else(|| Error::NotATable {
        path: shown.to_string(),
        key: key.to_string(),
    })?;

    let mut out = Alias::new();
    for (name, value) in aliases {
        // A target is relative to the file that declares it, and the resolver wants
        // somewhere it can start from rather than another specifier.
        let targets: Vec<AliasValue> = match value {
            toml::Value::String(one) => vec![absolute(dir, one)],
            toml::Value::Array(many) => many
                .iter()
                .map(|each| {
                    each.as_str()
                        .map(|each| absolute(dir, each))
                        .ok_or_else(|| Error::BadAlias {
                            path: shown.to_string(),
                            name: name.clone(),
                        })
                })
                .collect::<Result<_, _>>()?,
            _ => {
                return Err(Error::BadAlias {
                    path: shown.to_string(),
                    name: name.clone(),
                });
            }
        };
        out.push((name.clone(), targets));
    }
    out.sort_by_key(|(name, _)| specificity(name));
    Ok(out)
}

/// Sort key putting the most specific name first: an exact match, then the longest
/// literal prefix, then alphabetically so the order is settled.
fn specificity(name: &str) -> (u8, std::cmp::Reverse<usize>, String) {
    let exact = if name.ends_with('$') { 0 } else { 1 };
    let literal = name.split('*').next().unwrap_or(name).len();
    (exact, std::cmp::Reverse(literal), name.to_string())
}

fn flag(document: &toml::Table, key: &str, shown: &str) -> Result<Option<bool>, Error> {
    match document.get(key) {
        None => Ok(None),
        Some(value) => value.as_bool().map(Some).ok_or_else(|| Error::NotABoolean {
            path: shown.to_string(),
            key: key.to_string(),
        }),
    }
}

fn absolute(dir: &Path, target: &str) -> AliasValue {
    AliasValue::Path(dir.join(target).to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree of `fallout.toml` files, given as (directory, body) pairs.
    fn tree(files: &[(&str, &str)]) -> (tempfile::TempDir, Configs) {
        let dir = tempfile::tempdir().expect("temp dir");
        for (at, body) in files {
            let here = dir.path().join(at);
            std::fs::create_dir_all(&here).expect("a directory");
            std::fs::write(here.join(FILE), body).expect("writing the config");
        }
        let configs = Configs::new(dir.path());
        (dir, configs)
    }

    fn names(aliases: &Alias) -> Vec<String> {
        aliases.iter().map(|(name, _)| name.clone()).collect()
    }

    fn targets(aliases: &Alias) -> Vec<String> {
        aliases
            .iter()
            .flat_map(|(_, targets)| targets)
            .map(|value| match value {
                AliasValue::Path(path) => path.clone(),
                AliasValue::Ignore => "ignore".to_string(),
            })
            .collect()
    }

    #[test]
    fn no_file_is_no_claims() {
        let (dir, configs) = tree(&[]);
        let chain = configs.chain(&dir.path().join("src/page.scss"));
        assert!(chain.aliases().is_empty());
        assert!(chain.style_aliases().is_empty());
        assert!(!chain.inline_requires());
        assert!(configs.failure().is_none());
    }

    #[test]
    fn a_file_without_the_table_is_no_aliases() {
        let (dir, configs) = tree(&[("", "pure = [\"react#memo\"]\n")]);
        assert!(
            configs
                .chain(&dir.path().join("a.scss"))
                .style_aliases()
                .is_empty()
        );
    }

    #[test]
    fn an_alias_points_somewhere_below_the_file_that_declares_it() {
        let (dir, configs) = tree(&[("apps/web", "[style.aliases]\nsass = \"app/sass\"\n")]);
        let chain = configs.chain(&dir.path().join("apps/web/src/page.scss"));
        assert_eq!(names(chain.style_aliases()), vec!["sass".to_string()]);
        let target = &targets(chain.style_aliases())[0];
        assert!(
            target.ends_with("apps/web/app/sass") && target.starts_with('/'),
            "relative to its own file, not to the root: {target}"
        );
    }

    #[test]
    fn a_list_is_tried_in_the_order_it_is_written() {
        let (dir, configs) = tree(&[("", "[style.aliases]\nsass = [\"a/sass\", \"b/sass\"]\n")]);
        let found = targets(configs.chain(&dir.path().join("a.scss")).style_aliases());
        assert_eq!(found.len(), 2);
        assert!(found[0].ends_with("a/sass"), "{found:?}");
        assert!(found[1].ends_with("b/sass"), "{found:?}");
    }

    #[test]
    fn an_alias_is_not_visible_to_a_sibling_app() {
        let (dir, configs) = tree(&[
            ("apps/one", "[style.aliases]\nsass = \"styles\"\n"),
            ("apps/two", "[style.aliases]\nsass = \"scss\"\n"),
        ]);
        let one = targets(
            configs
                .chain(&dir.path().join("apps/one/a.scss"))
                .style_aliases(),
        );
        let two = targets(
            configs
                .chain(&dir.path().join("apps/two/a.scss"))
                .style_aliases(),
        );
        assert_eq!(one.len(), 1, "one app, one answer: {one:?}");
        assert!(one[0].ends_with("apps/one/styles"), "{one:?}");
        assert!(two[0].ends_with("apps/two/scss"), "{two:?}");
    }

    #[test]
    fn a_nearer_alias_is_tried_first_and_the_root_still_answers() {
        let (dir, configs) = tree(&[
            ("", "[style.aliases]\nsass = \"shared/sass\"\n"),
            ("apps/one", "[style.aliases]\nsass = \"styles\"\n"),
        ]);
        let found = targets(
            configs
                .chain(&dir.path().join("apps/one/a.scss"))
                .style_aliases(),
        );
        assert_eq!(found.len(), 2, "accumulated, not overridden: {found:?}");
        assert!(found[0].ends_with("apps/one/styles"), "nearest first");
        assert!(found[1].ends_with("shared/sass"));
    }

    #[test]
    fn a_general_alias_answers_any_specifier() {
        let (dir, configs) = tree(&[("apps/web", "[aliases]\n\"#app/*\" = \"app/*\"\n")]);
        let chain = configs.chain(&dir.path().join("apps/web/page.tsx"));
        assert_eq!(names(chain.aliases()), vec!["#app/*".to_string()]);
        assert!(targets(chain.aliases())[0].ends_with("apps/web/app/*"));
    }

    #[test]
    fn a_stylesheet_gets_the_general_table_too() {
        // A bundler has one `resolve.alias` and every import in the tree sees it. The
        // stylesheet-only table comes first so a name can still mean two things.
        let (dir, configs) = tree(&[(
            "apps/web",
            "[aliases]\nshared = \"vendor\"\n\n[style.aliases]\nsass = \"styles\"\n",
        )]);
        let chain = configs.chain(&dir.path().join("apps/web/page.scss"));
        assert_eq!(
            names(chain.style_aliases()),
            vec!["sass".to_string(), "shared".to_string()],
            "the stylesheet's own table is tried before the general one"
        );
        assert_eq!(
            names(chain.aliases()),
            vec!["shared".to_string()],
            "but a stylesheet-only name is not offered to JavaScript"
        );
    }

    #[test]
    fn the_more_specific_key_is_tried_first() {
        // TOML hands a table over alphabetically, and alphabetically `#app/*` comes
        // before `#app/assets/*`. Nobody writing both would mean the wildcard to win.
        let (dir, configs) = tree(&[(
            "",
            "[aliases]\n\"#app/*\" = \"app/*\"\n\"#app/assets/*\" = \"static/*\"\n\"#app/one$\" = \"one\"\n",
        )]);
        assert_eq!(
            names(configs.chain(&dir.path().join("page.tsx")).aliases()),
            vec![
                "#app/one$".to_string(),
                "#app/assets/*".to_string(),
                "#app/*".to_string(),
            ]
        );
    }

    #[test]
    fn a_general_alias_that_is_not_a_path_is_a_failure() {
        let (dir, configs) = tree(&[("", "[aliases]\nshared = 3\n")]);
        configs.chain(&dir.path().join("a.tsx"));
        assert!(matches!(configs.failure(), Some(Error::BadAlias { .. })));
    }

    #[test]
    fn a_general_table_that_is_not_a_table_is_a_failure() {
        let (dir, configs) = tree(&[("", "aliases = 3\n")]);
        configs.chain(&dir.path().join("a.tsx"));
        assert!(matches!(configs.failure(), Some(Error::NotATable { .. })));
    }

    #[test]
    fn a_setting_is_off_until_the_project_turns_it_on() {
        let (dir, configs) = tree(&[("apps/mobile", "inline-requires = true\n")]);
        assert!(configs.inline_requires(&[dir.path().join("apps/mobile/page.tsx")]));
        assert!(!configs.inline_requires(&[dir.path().join("apps/web/page.tsx")]));
        assert!(!configs.inline_requires(&[]), "no anchors claim nothing");
    }

    #[test]
    fn every_anchor_has_to_claim_it() {
        // It is the setting that drops edges, so one anchor that does not inline is
        // enough to turn it off: the alternative is under-reporting for that anchor.
        let (dir, configs) = tree(&[("apps/mobile", "inline-requires = true\n")]);
        let mobile = dir.path().join("apps/mobile/page.tsx");
        let web = dir.path().join("apps/web/page.tsx");
        assert!(configs.inline_requires(std::slice::from_ref(&mobile)));
        assert!(!configs.inline_requires(&[mobile, web]));
    }

    #[test]
    fn the_nearest_answer_wins_for_a_setting() {
        let (dir, configs) = tree(&[
            ("", "inline-requires = true\n"),
            ("apps/web", "inline-requires = false\n"),
        ]);
        assert!(configs.inline_requires(&[dir.path().join("apps/mobile/page.tsx")]));
        assert!(!configs.inline_requires(&[dir.path().join("apps/web/page.tsx")]));
    }

    #[test]
    fn a_pure_entry_applies_below_the_file_that_wrote_it() {
        let (dir, configs) = tree(&[("packages/ui", "pure = [\"./make#build\"]\n")]);
        let inside = configs.chain(&dir.path().join("packages/ui/card.tsx"));
        let outside = configs.chain(&dir.path().join("apps/web/page.tsx"));
        assert!(inside.pure().contains("./make", &["build"]));
        assert!(!outside.pure().contains("./make", &["build"]));
        // The built-ins are there either way.
        assert!(outside.pure().contains("react", &["memo"]));
    }

    #[test]
    fn a_target_that_is_not_a_path_is_a_failure() {
        for body in [
            "[style.aliases]\nsass = 3\n",
            "[style.aliases]\nsass = [\"ok\", 3]\n",
        ] {
            let (dir, configs) = tree(&[("", body)]);
            configs.chain(&dir.path().join("a.scss"));
            assert!(
                matches!(configs.failure(), Some(Error::BadAlias { .. })),
                "{body}"
            );
        }
    }

    #[test]
    fn a_table_that_is_not_a_table_is_a_failure() {
        for body in ["style = 3\n", "[style]\naliases = 3\n"] {
            let (dir, configs) = tree(&[("", body)]);
            configs.chain(&dir.path().join("a.scss"));
            assert!(
                matches!(configs.failure(), Some(Error::NotATable { .. })),
                "{body}"
            );
        }
    }

    #[test]
    fn a_setting_that_is_not_a_boolean_is_a_failure() {
        let (dir, configs) = tree(&[("", "inline-requires = \"yes\"\n")]);
        configs.chain(&dir.path().join("a.tsx"));
        assert!(matches!(configs.failure(), Some(Error::NotABoolean { .. })));
    }

    #[test]
    fn a_file_that_is_not_toml_is_a_failure() {
        let (dir, configs) = tree(&[("", "[style\n")]);
        configs.chain(&dir.path().join("a.scss"));
        assert!(matches!(configs.failure(), Some(Error::Unreadable { .. })));
    }

    #[test]
    fn a_bad_pure_entry_is_a_failure() {
        let (dir, configs) = tree(&[("", "pure = [\"memo\"]\n")]);
        configs.chain(&dir.path().join("a.tsx"));
        assert!(matches!(configs.failure(), Some(Error::Pure(_))));
    }

    #[test]
    fn a_file_the_run_never_needs_is_never_read() {
        // Lazy on purpose: a config in a subtree this run does not enter cannot have
        // changed the answer, so it is not this run's business to fail on it.
        let (dir, configs) = tree(&[("apps/other", "[style\n")]);
        configs.chain(&dir.path().join("apps/web/a.scss"));
        assert!(configs.failure().is_none());
        configs.chain(&dir.path().join("apps/other/a.scss"));
        assert!(configs.failure().is_some());
    }

    #[test]
    fn a_file_above_the_root_is_answered_by_the_root() {
        let (dir, configs) = tree(&[("", "inline-requires = true\n")]);
        let outside = dir.path().parent().expect("a parent").join("elsewhere.tsx");
        assert!(configs.inline_requires(&[outside]));
    }
}
