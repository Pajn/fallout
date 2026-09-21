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
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

pub fn analyse(options: &Options) -> Result<Outcome, Error> {
    let unresolved = std::sync::Arc::new(Unresolved::default());
    let verdict = analysed(options, &unresolved)?;
    Ok(Outcome {
        verdict,
        unresolved: unresolved.sorted(),
    })
}

fn analysed(options: &Options, unresolved: &std::sync::Arc<Unresolved>) -> Result<Verdict, Error> {
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
        let path = path.canonicalize().unwrap_or(path);
        if path.exists() {
            anchors.push(path);
        } else {
            missing.push(path.display().to_string());
        }
    }
    if !missing.is_empty() {
        return Err(Error::MissingAnchors(missing));
    }

    let change_set = options.diff.as_deref().map(diff::parse).unwrap_or_default();

    // Every `fallout.toml` at or below the root, read as the run reaches the files
    // each one speaks for. A run that analyses no module reads none of them.
    let configs = std::sync::Arc::new(config::Configs::new(&root));
    let reading = module::Reading {
        configs: configs.clone(),
        ignore_types: !options.include_types,
    };
    // A changed lockfile entry is a change to code this repository does not hold.
    // Read before the granularity split, because both searches ask one resolver and
    // it is the resolver that gives a changed package a node.
    let packages = std::sync::Arc::new(lockfile::changed(&root, &change_set, &options.changed));

    // Whether imports are deferred describes the bundler, and the anchors are what
    // pick one. Decided here, once, because the graph has no anchors of its own.
    let inline_requires = configs.inline_requires(&anchors);
    let base = options
        .base
        .as_deref()
        .map(|reference| base::Base::new(reference, reading.clone()));

    if options.granularity == Granularity::Symbol {
        let graph = graph::Graph::new(
            reading,
            inline_requires,
            unresolved.clone(),
            root.clone(),
            packages.clone(),
        );
        let verdict = analyse_symbols(&anchors, &root, &change_set, options, &graph, base.as_ref());
        return match configs.failure() {
            Some(error) => Err(Error::Config(error)),
            None => Ok(verdict),
        };
    }

    let changed = changes::marked_files(
        &root,
        &change_set,
        &options.changed,
        base.as_ref(),
        &reading,
    );

    if changed.is_empty() && packages.is_empty() {
        return Ok(Verdict::NotAffected);
    }

    let resolver = Resolver::new(
        configs.clone(),
        unresolved.clone(),
        root.clone(),
        packages.clone(),
    );

    let mut verdict = Verdict::NotAffected;
    if options.only != Some(Direction::Upstream)
        && let Some(hit) = query::downstream(&anchors, &changed, &resolver, &reading)
    {
        verdict = Verdict::Affected(hit);
    }
    if verdict == Verdict::NotAffected
        && options.only != Some(Direction::Downstream)
        && let Some(hit) = query::upstream(&anchors, &changed, &resolver, &reading)
    {
        verdict = Verdict::Affected(hit);
    }

    // A file that could not be read replaces the answer rather than shaping it.
    match configs.failure() {
        Some(error) => Err(Error::Config(error)),
        None => Ok(verdict),
    }
}

/// Declaration granularity. Only the downstream search narrows; upstream keeps its
/// file-level answer, so a symbol run is never *less* sensitive than a file run.
fn analyse_symbols(
    anchors: &[PathBuf],
    root: &Path,
    change_set: &diff::ChangeSet,
    options: &Options,
    graph: &graph::Graph,
    base: Option<&base::Base>,
) -> Verdict {
    let marked = marks::marked_nodes(graph, change_set, root, &options.changed, base);

    if marked.is_empty() && !graph.resolver().has_changed_packages() {
        return Verdict::NotAffected;
    }

    if options.only != Some(Direction::Upstream)
        && let Some(nodes) = query::downstream_symbols(anchors, &marked, graph)
    {
        let path = nodes.iter().map(|node| graph.path(node.file())).collect();
        let rendered = nodes.iter().map(|node| graph.render(*node, root)).collect();
        return Verdict::Affected(Hit {
            direction: Direction::Downstream,
            rendered: Some(rendered),
            path,
        });
    }

    if options.only != Some(Direction::Downstream) {
        let changed =
            changes::marked_files(root, change_set, &options.changed, base, graph.reading());
        if let Some(hit) = query::upstream(anchors, &changed, graph.resolver(), graph.reading()) {
            return Verdict::Affected(hit);
        }
    }

    Verdict::NotAffected
}
