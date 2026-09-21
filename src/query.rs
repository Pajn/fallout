//! Graph traversal in both directions, capturing the path that produced a hit.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use ahash::{AHashMap, AHashSet};
use clap::ValueEnum;

use crate::graph::{Graph, Node};
use crate::module::{Reading, imported_specifiers};
use crate::resolve::Resolver;

/// Which way to walk the import graph between the anchor and a changed file.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Direction {
    /// The anchor imports the changed file, directly or transitively
    Downstream,
    /// The changed file imports the anchor, directly or transitively
    Upstream,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Downstream => "downstream",
            Direction::Upstream => "upstream",
        }
    }
}

/// A proven chain from one end of the search to the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub direction: Direction,
    /// Node labels, set when the search ran over the node graph rather than over
    /// whole files. Rendered eagerly because the graph owns the name interner.
    pub rendered: Option<Vec<String>>,
    /// The chain in import order: each file imports the next.
    ///
    /// Downstream that reads anchor first, changed file last; upstream the reverse,
    /// because upstream means the changed file is the one doing the importing.
    pub path: Vec<PathBuf>,
}

impl Hit {
    /// The file whose change produced this verdict.
    pub fn changed_file(&self) -> &Path {
        match self.direction {
            Direction::Downstream => self.path.last().expect("a hit has at least one file"),
            Direction::Upstream => self.path.first().expect("a hit has at least one file"),
        }
    }
}

/// Walks forward from every anchor, looking for a changed file.
pub fn downstream(
    anchors: &[PathBuf],
    changed: &AHashSet<PathBuf>,
    resolver: &Resolver,
    reading: &Reading,
) -> Option<Hit> {
    let mut came_from: AHashMap<PathBuf, Option<PathBuf>> = AHashMap::default();
    let mut queue = VecDeque::new();

    for anchor in anchors {
        if came_from.insert(anchor.clone(), None).is_none() {
            queue.push_back(anchor.clone());
        }
    }

    while let Some(current) = queue.pop_front() {
        if changed.contains(&current) {
            return Some(Hit {
                direction: Direction::Downstream,
                rendered: None,
                path: trace(&came_from, &current),
            });
        }

        for next in edges_from(&current, resolver, reading) {
            if !came_from.contains_key(&next) {
                came_from.insert(next.clone(), Some(current.clone()));
                queue.push_back(next);
            }
        }
    }

    None
}

/// Walks forward from every changed file, looking for an anchor.
///
/// Roots are visited in a stable order so that a run with several changed files
/// always reports the same one.
pub fn upstream(
    anchors: &[PathBuf],
    changed: &AHashSet<PathBuf>,
    resolver: &Resolver,
    reading: &Reading,
) -> Option<Hit> {
    let anchor_set: AHashSet<&PathBuf> = anchors.iter().collect();

    let mut roots: Vec<&PathBuf> = changed.iter().collect();
    roots.sort();

    for root in roots {
        let mut came_from: AHashMap<PathBuf, Option<PathBuf>> = AHashMap::default();
        let mut queue = VecDeque::new();
        came_from.insert(root.clone(), None);
        queue.push_back(root.clone());

        while let Some(current) = queue.pop_front() {
            if anchor_set.contains(&current) {
                return Some(Hit {
                    direction: Direction::Upstream,
                    rendered: None,
                    path: trace(&came_from, &current),
                });
            }

            for next in edges_from(&current, resolver, reading) {
                if !came_from.contains_key(&next) {
                    came_from.insert(next.clone(), Some(current.clone()));
                    queue.push_back(next);
                }
            }
        }
    }

    None
}

fn edges_from(file: &Path, resolver: &Resolver, reading: &Reading) -> Vec<PathBuf> {
    let Some(specifiers) = imported_specifiers(file, reading) else {
        return Vec::new();
    };

    specifiers
        .iter()
        .filter_map(|specifier| resolver.resolve(file, specifier))
        .collect()
}

/// Rebuilds the chain from a root to `target`, root first.
fn trace(came_from: &AHashMap<PathBuf, Option<PathBuf>>, target: &Path) -> Vec<PathBuf> {
    let mut path = vec![target.to_path_buf()];
    let mut cursor = target.to_path_buf();

    while let Some(Some(previous)) = came_from.get(&cursor) {
        path.push(previous.clone());
        cursor = previous.clone();
    }

    path.reverse();
    path
}

/// Walks forward from the anchors over the node graph, looking for a marked node.
///
/// Only the downstream direction narrows. Upstream narrowing has a definitional
/// problem — a change to a sibling component cannot reach a page through references,
/// yet they render together — so upstream stays at file granularity until that
/// question is settled.
pub fn downstream_symbols(
    anchors: &[PathBuf],
    marked: &AHashSet<Node>,
    graph: &Graph,
) -> Option<Vec<Node>> {
    let mut came_from: AHashMap<Node, Option<Node>> = AHashMap::default();
    let mut queue = VecDeque::new();

    for anchor in anchors {
        let node = Node::File(graph.file_id(anchor));
        if came_from.insert(node, None).is_none() {
            queue.push_back(node);
        }
    }

    while let Some(current) = queue.pop_front() {
        if is_marked(marked, current) {
            return Some(trace_nodes(&came_from, current));
        }

        for next in graph.edges(current) {
            if !came_from.contains_key(&next) {
                came_from.insert(next, Some(current));
                queue.push_back(next);
            }
        }
    }

    None
}

/// `File(f)` is the umbrella node: marking it says "something in f changed, and we
/// cannot say what". Every node of `f` is therefore marked with it, or a search that
/// reaches only a declaration would miss a whole-file change.
fn is_marked(marked: &AHashSet<Node>, node: Node) -> bool {
    marked.contains(&node) || marked.contains(&Node::File(node.file()))
}

fn trace_nodes(came_from: &AHashMap<Node, Option<Node>>, target: Node) -> Vec<Node> {
    let mut path = vec![target];
    let mut cursor = target;
    while let Some(Some(previous)) = came_from.get(&cursor) {
        path.push(*previous);
        cursor = *previous;
    }
    path.reverse();
    path
}
