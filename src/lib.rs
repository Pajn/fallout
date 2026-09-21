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
pub mod diff;
pub mod graph;
pub mod marks;
pub mod module;
pub mod pure;
pub mod query;
pub mod resolve;

use std::fmt;
use std::path::{Path, PathBuf};

use crate::query::{Direction, Hit};

use crate::resolve::Resolver;

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
    /// The project's `fallout.toml` is there but could not be read. Reported rather
    /// than ignored: a list of pure callees that silently does nothing would show up
    /// as a verdict nobody can explain.
    Config(pure::Error),
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

pub fn analyse(options: &Options) -> Result<Verdict, Error> {
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

    // Both the declaration walk and the base comparison analyse modules, and both ask
    // what the project calls pure. A run that does neither has no reason to read the
    // list, nor to fail on one it cannot.
    let pure = if options.granularity == Granularity::Symbol || options.base.is_some() {
        pure::PureList::load(&root).map_err(Error::Config)?
    } else {
        pure::PureList::default()
    };
    let reading = module::Reading {
        pure,
        ignore_types: !options.include_types,
    };
    let base = options
        .base
        .as_deref()
        .map(|reference| base::Base::new(reference, reading.clone()));

    if options.granularity == Granularity::Symbol {
        return Ok(analyse_symbols(
            &anchors,
            &root,
            &change_set,
            options,
            reading,
            base.as_ref(),
        ));
    }

    let changed = changes::marked_files(
        &root,
        &change_set,
        &options.changed,
        base.as_ref(),
        &reading,
    );

    if changed.is_empty() {
        return Ok(Verdict::NotAffected);
    }

    let resolver = Resolver::new();

    if options.only != Some(Direction::Upstream) {
        if let Some(hit) = query::downstream(&anchors, &changed, &resolver, &reading) {
            return Ok(Verdict::Affected(hit));
        }
    }

    if options.only != Some(Direction::Downstream) {
        if let Some(hit) = query::upstream(&anchors, &changed, &resolver, &reading) {
            return Ok(Verdict::Affected(hit));
        }
    }

    Ok(Verdict::NotAffected)
}

/// Declaration granularity. Only the downstream search narrows; upstream keeps its
/// file-level answer, so a symbol run is never *less* sensitive than a file run.
fn analyse_symbols(
    anchors: &[PathBuf],
    root: &Path,
    change_set: &diff::ChangeSet,
    options: &Options,
    reading: module::Reading,
    base: Option<&base::Base>,
) -> Verdict {
    let graph = graph::Graph::new(reading);
    let marked = marks::marked_nodes(&graph, change_set, root, &options.changed, base);

    if marked.is_empty() {
        return Verdict::NotAffected;
    }

    if options.only != Some(Direction::Upstream) {
        if let Some(nodes) = query::downstream_symbols(anchors, &marked, &graph) {
            let path = nodes.iter().map(|node| graph.path(node.file())).collect();
            let rendered = nodes.iter().map(|node| graph.render(*node, root)).collect();
            return Verdict::Affected(Hit {
                direction: Direction::Downstream,
                rendered: Some(rendered),
                path,
            });
        }
    }

    if options.only != Some(Direction::Downstream) {
        let changed =
            changes::marked_files(root, change_set, &options.changed, base, graph.reading());
        let resolver = Resolver::new();
        if let Some(hit) = query::upstream(anchors, &changed, &resolver, graph.reading()) {
            return Verdict::Affected(hit);
        }
    }

    Verdict::NotAffected
}
