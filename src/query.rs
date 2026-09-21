//! Graph traversal in both directions, capturing the path that produced a hit.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use ahash::{AHashMap, AHashSet};
use clap::ValueEnum;

use crate::module::imported_specifiers;
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
                path: trace(&came_from, &current),
            });
        }

        for next in edges_from(&current, resolver) {
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
                    path: trace(&came_from, &current),
                });
            }

            for next in edges_from(&current, resolver) {
                if !came_from.contains_key(&next) {
                    came_from.insert(next.clone(), Some(current.clone()));
                    queue.push_back(next);
                }
            }
        }
    }

    None
}

fn edges_from(file: &Path, resolver: &Resolver) -> Vec<PathBuf> {
    let Some(specifiers) = imported_specifiers(file) else {
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
