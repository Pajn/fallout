//! `fallout` answers one question: can this change alter what a user sees on a given
//! page?
//!
//! The unit of analysis is currently the file. Any change anywhere in a file marks
//! every importer of that file, transitively. See `docs/symbol-level-analysis.md` for
//! how that is being refined, and for the soundness contract every refinement must
//! honour: the tool may over-report, it may never under-report.

pub mod base;
pub mod changes;
pub mod cli;
pub mod config;
pub mod diff;
pub mod graph;
pub mod lockfile;
pub mod marks;
pub mod module;
pub mod pure;
pub mod query;
pub mod resolve;

use std::fmt;
use std::path::{Path, PathBuf};

use crate::query::{Direction, Hit};

use crate::resolve::{Resolver, Unresolved};

/// How finely the analysis distinguishes parts of a file.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Granularity {
    /// Any change anywhere in a file marks the whole file.
    #[default]
    File,
    /// Changes are attributed to individual declarations, exports and module
    /// initialisation. Any module the analyser cannot describe falls back to one
    /// opaque node, which is the `File` behaviour.
    Symbol,
}

impl Granularity {
    pub fn as_str(self) -> &'static str {
        match self {
            Granularity::File => "file",
            Granularity::Symbol => "symbol",
        }
    }
}

/// One run of the analysis.
pub struct Options {
    /// Files to judge. The set is affected if any one of them is.
    pub anchors: Vec<PathBuf>,
    /// Changed paths given directly, as from `git diff --name-only`.
    pub changed: Vec<PathBuf>,
    /// A unified diff describing the change.
    pub diff: Option<String>,
    /// Revision the change is measured against. With one, a file is compared
    /// against its earlier self as syntax rather than as lines, so a reformatting
    /// or a reworded comment marks nothing.
    pub base: Option<String>,
    /// Directory the anchors and diff paths are relative to.
    pub root: PathBuf,
    /// Search only this direction instead of both.
    pub only: Option<Direction>,
    pub granularity: Granularity,
    /// Read every file as it was written, types and all, so that a change made only
    /// of types still counts as a change. Off by default: a type error fails the
    /// build for every page at once, which is a different question from the one this
    /// tool answers, and reachability is no help with it.
    pub include_types: bool,
}

/// One run's answer, and what it noticed on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub verdict: Verdict,
    /// Specifiers the run asked for and could not place on disk, each with the files
    /// that wrote it. Reported only when asked for: see [`resolve::Unresolved`].
    pub unresolved: Vec<(String, Vec<PathBuf>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Affected(Hit),
    NotAffected,
}

impl Verdict {
    pub fn is_affected(&self) -> bool {
        matches!(self, Verdict::Affected(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    NoAnchors,
    MissingAnchors(Vec<String>),
    /// A `fallout.toml` the run needed is there but could not be read. Reported
    /// rather than ignored: a setting that silently does nothing would show up as a
    /// verdict nobody can explain.
    Config(config::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoAnchors => write!(f, "At least one anchor must be provided"),
            Error::MissingAnchors(paths) => {
                write!(f, "Anchor(s) not found: {}", paths.join(", "))
            }
            Error::Config(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {}

/// Resolves `root` to an absolute path, falling back to it unchanged when it does not
/// exist. Shared with the CLI so that displayed paths and analysed paths agree.
pub fn canonical_root(root: &Path) -> PathBuf {
    dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

pub fn analyse(options: &Options) -> Result<Outcome, Error> {
    let run = Run::new(options)?;
    // The set is affected if any one of its anchors is. Anchors whose bundlers agree
    // are asked together, on one graph; the first group affected is the answer.
    let mut verdict = Verdict::NotAffected;
    for (bundler, anchors) in run.groups() {
        let engine = run.engine(bundler);
        let judged = engine.judge(&run, &anchors);
        if judged.verdict.is_affected() {
            verdict = judged.verdict;
            break;
        }
    }
    run.finish()?;
    Ok(Outcome {
        verdict,
        unresolved: run.unresolved.sorted(),
    })
}

/// One anchor's answer, as [`analyse_each`] gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorOutcome {
    pub anchor: PathBuf,
    pub verdict: Verdict,
    /// Specifiers the searches for this anchor reached and could not place on disk,
    /// each with the files that wrote it and what kind of name it is.
    ///
    /// For an anchor found not affected this covers everything its searches could
    /// reach, which is every place a missing edge could have hidden a change from it.
    /// For one found affected it covers what the search saw before it stopped.
    pub unresolved: Vec<UnresolvedImport>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedImport {
    pub specifier: String,
    pub from: Vec<PathBuf>,
    pub kind: resolve::UnresolvedKind,
}

/// Answers each anchor on its own, in one run.
///
/// Every anchor sharing a bundler is answered on the same graph, so each module is
/// read and resolved once however many anchors reach it.
pub fn analyse_each(options: &Options) -> Result<Vec<AnchorOutcome>, Error> {
    let run = Run::new(options)?;
    let mut answers: Vec<AnchorOutcome> = Vec::with_capacity(run.anchors.len());
    for (bundler, anchors) in run.groups() {
        let engine = run.engine(bundler);
        for anchor in anchors {
            let judged = engine.judge(&run, std::slice::from_ref(&anchor));
            let unresolved = run
                .unresolved
                .sorted()
                .into_iter()
                .filter_map(|(specifier, from)| {
                    let from: Vec<PathBuf> = from
                        .into_iter()
                        .filter(|file| judged.visited.contains(file))
                        .collect();
                    let kind = engine.resolver().unresolved_kind(from.first()?, &specifier);
                    Some(UnresolvedImport {
                        specifier,
                        from,
                        kind,
                    })
                })
                .collect();
            answers.push(AnchorOutcome {
                anchor,
                verdict: judged.verdict,
                unresolved,
            });
        }
    }
    run.finish()?;
    // In the order the anchors were given, whichever group each fell in.
    answers.sort_by_key(|answer| {
        run.anchors
            .iter()
            .position(|anchor| *anchor == answer.anchor)
    });
    Ok(answers)
}

/// What every question in one run shares: the anchors, the change, and the
/// project's own settings.
struct Run<'o> {
    options: &'o Options,
    root: PathBuf,
    anchors: Vec<PathBuf>,
    change_set: diff::ChangeSet,
    configs: std::sync::Arc<config::Configs>,
    reading: module::Reading,
    packages: std::sync::Arc<lockfile::Changed>,
    base: Option<base::Base>,
    unresolved: std::sync::Arc<Unresolved>,
    /// The files the change marks, for the searches that work file by file: every
    /// file-level search, and the upstream one at any granularity. Worked out once,
    /// since nothing about it depends on the bundler.
    changed: std::cell::OnceCell<ahash::AHashSet<PathBuf>>,
}

impl<'o> Run<'o> {
    fn new(options: &'o Options) -> Result<Self, Error> {
        if options.anchors.is_empty() {
            return Err(Error::NoAnchors);
        }

        let root = canonical_root(&options.root);

        let mut anchors = Vec::new();
        let mut missing = Vec::new();
        for anchor in &options.anchors {
            let path = if anchor.is_relative() {
                root.join(anchor)
            } else {
                anchor.clone()
            };
            let path = dunce::canonicalize(&path).unwrap_or(path);
            if path.exists() {
                if !anchors.contains(&path) {
                    anchors.push(path);
                }
            } else {
                missing.push(path.display().to_string());
            }
        }
        if !missing.is_empty() {
            return Err(Error::MissingAnchors(missing));
        }

        let change_set = options.diff.as_deref().map(diff::parse).unwrap_or_default();

        // Every `fallout.toml` at or below the root, read as the run reaches the
        // files each one speaks for. A run that analyses no module reads none of them.
        let configs = std::sync::Arc::new(config::Configs::new(&root));
        let reading = module::Reading {
            configs: configs.clone(),
            ignore_types: !options.include_types,
        };
        // A changed lockfile entry is a change to code this repository does not
        // hold. Read before any search, because every search asks a resolver and it
        // is the resolver that gives a changed package a node.
        let packages = std::sync::Arc::new(lockfile::changed(&root, &change_set, &options.changed));
        let base = options
            .base
            .as_deref()
            .map(|reference| base::Base::new(reference, reading.clone()));

        Ok(Self {
            options,
            root,
            anchors,
            change_set,
            configs,
            reading,
            packages,
            base,
            unresolved: std::sync::Arc::new(Unresolved::default()),
            changed: std::cell::OnceCell::new(),
        })
    }

    /// The anchors grouped by what their apps' bundlers do, in the order each group's
    /// first anchor was given.
    ///
    /// The bundler is a property of the app an anchor belongs to rather than of any
    /// file, so it is read from the chain above the anchor. See [`config`].
    fn groups(&self) -> Vec<(config::Bundler, Vec<PathBuf>)> {
        let mut groups: Vec<(config::Bundler, Vec<PathBuf>)> = Vec::new();
        for anchor in &self.anchors {
            let bundler = self.configs.bundler(anchor);
            match groups.iter_mut().find(|(known, _)| *known == bundler) {
                Some((_, members)) => members.push(anchor.clone()),
                None => groups.push((bundler, vec![anchor.clone()])),
            }
        }
        groups
    }

    fn changed(&self, reading: &module::Reading) -> &ahash::AHashSet<PathBuf> {
        self.changed.get_or_init(|| {
            changes::marked_files(
                &self.root,
                &self.change_set,
                &self.options.changed,
                self.base.as_ref(),
                reading,
            )
        })
    }

    fn engine(&self, bundler: config::Bundler) -> Engine {
        match self.options.granularity {
            Granularity::File => Engine::File(Box::new(Resolver::new(
                self.configs.clone(),
                self.unresolved.clone(),
                self.root.clone(),
                self.packages.clone(),
                bundler.lookup,
            ))),
            Granularity::Symbol => {
                let graph = graph::Graph::new(
                    self.reading.clone(),
                    bundler,
                    self.unresolved.clone(),
                    self.root.clone(),
                    self.packages.clone(),
                );
                let marked = marks::marked_nodes(
                    &graph,
                    &self.change_set,
                    &self.root,
                    &self.options.changed,
                    self.base.as_ref(),
                );
                Engine::Symbol(Box::new((graph, marked)))
            }
        }
    }

    /// A file that could not be read replaces the answer rather than shaping it.
    fn finish(&self) -> Result<(), Error> {
        match self.configs.failure() {
            Some(error) => Err(Error::Config(error)),
            None => Ok(()),
        }
    }
}

/// What answers the anchors of one bundler: a resolver for a file-level run, a node
/// graph and what the change marks in it for a declaration-level one.
enum Engine {
    File(Box<Resolver>),
    Symbol(Box<(graph::Graph, ahash::AHashSet<graph::Node>)>),
}

struct Judged {
    verdict: Verdict,
    /// Every file the searches looked at.
    visited: ahash::AHashSet<PathBuf>,
}

impl Engine {
    fn resolver(&self) -> &Resolver {
        match self {
            Engine::File(resolver) => resolver,
            Engine::Symbol(symbol) => symbol.0.resolver(),
        }
    }

    fn judge(&self, run: &Run<'_>, anchors: &[PathBuf]) -> Judged {
        let only = run.options.only;
        let mut visited = ahash::AHashSet::default();
        let affected = |hit: Hit, visited| Judged {
            verdict: Verdict::Affected(hit),
            visited,
        };

        match self {
            Engine::File(resolver) => {
                let changed = run.changed(&run.reading);
                if changed.is_empty() && run.packages.is_empty() {
                    return Judged {
                        verdict: Verdict::NotAffected,
                        visited,
                    };
                }
                if only != Some(Direction::Upstream) {
                    let search = query::downstream(anchors, changed, resolver, &run.reading);
                    visited.extend(search.visited);
                    if let Some(hit) = search.hit {
                        return affected(hit, visited);
                    }
                }
                if only != Some(Direction::Downstream) {
                    let search = query::upstream(anchors, changed, resolver, &run.reading);
                    visited.extend(search.visited);
                    if let Some(hit) = search.hit {
                        return affected(hit, visited);
                    }
                }
            }
            // Only the downstream search narrows; upstream keeps its file-level
            // answer, so a declaration run is never *less* sensitive than a file run.
            Engine::Symbol(symbol) => {
                let (graph, marked) = symbol.as_ref();
                if marked.is_empty() && !graph.resolver().has_changed_packages() {
                    return Judged {
                        verdict: Verdict::NotAffected,
                        visited,
                    };
                }
                if only != Some(Direction::Upstream) {
                    let search = query::downstream_symbols(anchors, marked, graph);
                    visited.extend(search.visited);
                    if let Some(nodes) = search.hit {
                        let path = nodes.iter().map(|node| graph.path(node.file())).collect();
                        let rendered = nodes
                            .iter()
                            .map(|node| graph.render(*node, &run.root))
                            .collect();
                        let hit = Hit {
                            direction: Direction::Downstream,
                            rendered: Some(rendered),
                            path,
                        };
                        return affected(hit, visited);
                    }
                }
                if only != Some(Direction::Downstream) {
                    let changed = run.changed(graph.reading());
                    let search =
                        query::upstream(anchors, changed, graph.resolver(), graph.reading());
                    visited.extend(search.visited);
                    if let Some(hit) = search.hit {
                        return affected(hit, visited);
                    }
                }
            }
        }
        Judged {
            verdict: Verdict::NotAffected,
            visited,
        }
    }
}
