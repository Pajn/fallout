//! `fallout` answers one question: can this change alter what a user sees on a given
//! page?
//!
//! The unit of analysis is currently the file. Any change anywhere in a file marks
//! every importer of that file, transitively. See `docs/symbol-level-analysis.md` for
//! how that is being refined, and for the soundness contract every refinement must
//! honour: the tool may over-report, it may never under-report.

pub mod base;
pub mod change;
pub mod cli;
pub mod config;
pub mod diff;
pub mod factories;
pub mod graph;
pub mod lockfile;
pub mod marks;
#[cfg(test)]
mod memory_fs;
pub mod module;
pub mod pure;
pub mod query;
pub mod repoint;
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
    /// Git finds no commit by the `--base` revision from the run's root. Refused
    /// rather than read: a revision with no files in it would have every changed
    /// file taken as one the change added.
    UnknownBase {
        revision: String,
        root: PathBuf,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoAnchors => write!(f, "At least one anchor must be provided"),
            Error::MissingAnchors(paths) => {
                write!(f, "Anchor(s) not found: {}", paths.join(", "))
            }
            Error::Config(error) => write!(f, "{error}"),
            Error::UnknownBase { revision, root } => write!(
                f,
                "Base revision not found: git finds no commit named {revision} from {}",
                root.display()
            ),
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
    analyse_with(options, earlier(options)?)
}

/// As [`analyse`], with the earlier versions of the files handed over rather than
/// read from the revision `options.base` names, which is ignored. With `None` there
/// are none, and the change is read from the diff and the changed paths alone.
///
/// This is how a caller with no repository to hand, a test say, gives a run what a
/// `--base` run would read from git. `earlier` has to hold every text file of the
/// tree before the change: a file it has no text for is taken as one the change
/// added. See [`base::Earlier::text`].
pub fn analyse_with(
    options: &Options,
    earlier: Option<Box<dyn base::Earlier>>,
) -> Result<Outcome, Error> {
    analysed(options, earlier).map(|(outcome, _)| outcome)
}

/// The earlier versions `options.base` names, read from git, once git has said it
/// knows the revision. One it does not know is no answer rather than a tree with
/// nothing in it. See [`base::Base::is_known`].
pub(crate) fn earlier(options: &Options) -> Result<Option<Box<dyn base::Earlier>>, Error> {
    let Some(reference) = options.base.as_deref() else {
        return Ok(None);
    };
    let base = base::Base::new(reference);
    let root = canonical_root(&options.root);
    if !base.is_known(&root) {
        return Err(Error::UnknownBase {
            revision: reference.to_string(),
            root,
        });
    }
    Ok(Some(Box::new(base)))
}

/// [`analyse_with`]'s answer, and the root the run measured it from, so that the
/// command line displays paths against the same directory the analysis read.
pub(crate) fn analysed(
    options: &Options,
    earlier: Option<Box<dyn base::Earlier>>,
) -> Result<(Outcome, PathBuf), Error> {
    let run = Run::new(options, earlier)?;
    // The set is affected if any one of its anchors is. Anchors whose bundlers agree
    // are asked together, on one graph; the first group affected is the answer.
    let mut verdict = Verdict::NotAffected;
    for (bundler, anchors) in run.groups() {
        let engine = run.engine(bundler);
        let judged = engine.judge(&run, &anchors);
        run.unresolved.absorb(engine.resolver().unresolved());
        if judged.verdict.is_affected() {
            verdict = judged.verdict;
            break;
        }
    }
    run.finish()?;
    let outcome = Outcome {
        verdict,
        unresolved: run.unresolved.sorted(),
    };
    Ok((outcome, run.root))
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
    analyse_each_with(options, earlier(options)?)
}

/// As [`analyse_each`], with the earlier versions of the files handed over rather
/// than read from the revision `options.base` names, which is ignored. See
/// [`analyse_with`].
pub fn analyse_each_with(
    options: &Options,
    earlier: Option<Box<dyn base::Earlier>>,
) -> Result<Vec<AnchorOutcome>, Error> {
    analysed_each(options, earlier).map(|(answers, _)| answers)
}

/// [`analyse_each_with`]'s answers, and the root the run measured them from.
pub(crate) fn analysed_each(
    options: &Options,
    earlier: Option<Box<dyn base::Earlier>>,
) -> Result<(Vec<AnchorOutcome>, PathBuf), Error> {
    let run = Run::new(options, earlier)?;
    let mut answers: Vec<AnchorOutcome> = Vec::with_capacity(run.anchors.len());
    for (bundler, anchors) in run.groups() {
        let engine = run.engine(bundler);
        for anchor in anchors {
            let judged = engine.judge(&run, std::slice::from_ref(&anchor));
            // Only what this anchor's own bundler failed to place, and only where its
            // own searches went. A name several files write is as in-repo as the most
            // in-repo of them says, since each file resolves it in its own chain.
            let unresolved = engine
                .resolver()
                .unresolved()
                .sorted()
                .into_iter()
                .filter_map(|(specifier, from)| {
                    let from: Vec<PathBuf> = from
                        .into_iter()
                        .filter(|file| judged.visited.contains(file))
                        .collect();
                    let kind = from
                        .iter()
                        .map(|file| engine.resolver().unresolved_kind(file, &specifier))
                        .min()?;
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
    Ok((answers, run.root))
}

/// What every question in one run shares: the anchors, the change, and the
/// project's own settings.
struct Run<'o> {
    options: &'o Options,
    root: PathBuf,
    anchors: Vec<PathBuf>,
    /// Nothing about it depends on the bundler, so every group asks the same one.
    change: change::Change,
    configs: std::sync::Arc<config::Configs>,
    reading: module::Reading,
    /// Every group's unresolved specifiers together, for the combined report. Each
    /// group records its own, since what one bundler cannot place another may.
    unresolved: Unresolved,
}

impl<'o> Run<'o> {
    fn new(options: &'o Options, earlier: Option<Box<dyn base::Earlier>>) -> Result<Self, Error> {
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
        let change = change::Change::read(
            &root,
            change_set,
            &options.changed,
            earlier,
            reading.clone(),
        );

        Ok(Self {
            options,
            root,
            anchors,
            change,
            configs,
            reading,
            unresolved: Unresolved::default(),
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

    fn engine(&self, bundler: config::Bundler) -> Engine<'_> {
        let (packages, repointing) = self.change.for_resolution();
        match self.options.granularity {
            Granularity::File => Engine::File(Box::new(query::FileGraph::new(
                std::sync::Arc::new(Resolver::new(
                    self.configs.clone(),
                    std::sync::Arc::new(Unresolved::default()),
                    self.root.clone(),
                    packages,
                    repointing,
                    bundler.lookup,
                )),
            ))),
            Granularity::Symbol => {
                let graph = graph::Graph::new(
                    self.reading.clone(),
                    bundler,
                    std::sync::Arc::new(Unresolved::default()),
                    self.root.clone(),
                    packages,
                    repointing,
                    self.change.lost_exports(),
                );
                let marks = marks::Marks::new(&graph, &self.change);
                // Built once for the engine rather than once per search, so that the
                // upstream searches of several anchors read each file once between
                // them.
                let files = graph.file_graph();
                Engine::Symbol(Box::new((graph, marks, files)))
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

/// What answers the anchors of one bundler: the imports file by file for a
/// file-level run; for a declaration-level one, a node graph, what the change marks
/// in it, and the imports file by file over the same resolver for the upstream
/// search.
enum Engine<'c> {
    File(Box<query::FileGraph>),
    Symbol(Box<(graph::Graph, marks::Marks<'c>, query::FileGraph)>),
}

struct Judged {
    verdict: Verdict,
    /// Every file the searches looked at.
    visited: ahash::AHashSet<PathBuf>,
}

impl Engine<'_> {
    fn resolver(&self) -> &Resolver {
        match self {
            Engine::File(files) => files.resolver(),
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

        if run.change.is_empty() {
            return Judged {
                verdict: Verdict::NotAffected,
                visited,
            };
        }
        match self {
            Engine::File(files) => {
                let changed = run.change.files();
                if only != Some(Direction::Upstream) {
                    let search = query::downstream(anchors, &run.change, files, &run.reading);
                    visited.extend(search.visited);
                    if let Some(hit) = search.hit {
                        return affected(hit, visited);
                    }
                }
                if only != Some(Direction::Downstream) {
                    let search = query::upstream(anchors, changed, files, &run.reading);
                    visited.extend(search.visited);
                    if let Some(hit) = search.hit {
                        return affected(hit, visited);
                    }
                }
            }
            // Only the downstream search narrows; upstream keeps its file-level
            // answer, so a declaration run is never *less* sensitive than a file run.
            Engine::Symbol(symbol) => {
                let (graph, marks, files) = symbol.as_ref();
                if only != Some(Direction::Upstream) {
                    let search = query::downstream_symbols(anchors, marks, graph);
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
                    let search =
                        query::upstream(anchors, run.change.files(), files, graph.reading());
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
