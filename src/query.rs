//! Graph traversal in both directions, capturing the path that produced a hit.

use std::cell::{OnceCell, RefCell};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use ahash::{AHashMap, AHashSet};
use clap::ValueEnum;
use rayon::prelude::*;

use crate::change::Change;
use crate::graph::{Graph, Node};
use crate::marks::Marks;
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

/// What a search found, and every file it looked at on the way.
///
/// A search that found nothing has looked at everything it could reach, so the
/// files it visited are all the places an import it could not place might have
/// hidden an edge. One that found something stopped early, and says so only about
/// what it saw.
#[derive(Debug, Default)]
pub struct Search<T> {
    pub hit: Option<T>,
    pub visited: AHashSet<PathBuf>,
}

/// The import graph file by file, as one bundler's resolver resolves it, for the
/// searches that walk whole files.
pub struct FileGraph {
    resolver: Arc<Resolver>,
    /// What each file imports, resolved. Several anchors' searches walk the same
    /// files, and reading one means parsing it.
    imports: RefCell<AHashMap<PathBuf, Rc<[PathBuf]>>>,
}

impl FileGraph {
    /// Whether `file`'s imports are already resolved, so reading it would be wasted.
    fn knows(&self, file: &Path) -> bool {
        self.imports.borrow().contains_key(file)
    }

    pub fn new(resolver: Arc<Resolver>) -> Self {
        Self {
            resolver,
            imports: RefCell::new(AHashMap::default()),
        }
    }

    /// The resolver the edges are resolved with.
    pub fn resolver(&self) -> &Resolver {
        &self.resolver
    }

    /// The files `file` imports, resolving `specifiers`, which are read only the
    /// first time `file` is asked about.
    pub fn edges_from<S: AsRef<[String]>>(
        &self,
        file: &Path,
        specifiers: impl FnOnce() -> S,
    ) -> Rc<[PathBuf]> {
        if let Some(known) = self.imports.borrow().get(file) {
            return known.clone();
        }
        let imports: Rc<[PathBuf]> = specifiers()
            .as_ref()
            .iter()
            .flat_map(|specifier| self.resolver.resolve(file, specifier).to_vec())
            .collect();
        self.imports
            .borrow_mut()
            .insert(file.to_path_buf(), imports.clone());
        imports
    }
}

/// Walks forward from every anchor, looking for a changed file.
pub fn downstream(
    anchors: &[PathBuf],
    change: &Change,
    files: &FileGraph,
    reading: &Reading,
) -> Search<Hit> {
    let mut came_from: AHashMap<PathBuf, Option<PathBuf>> = AHashMap::default();
    let mut queue = VecDeque::new();

    for anchor in anchors {
        if came_from.insert(anchor.clone(), None).is_none() {
            queue.push_back(anchor.clone());
        }
    }

    let mut read: AHashMap<PathBuf, Vec<String>> = AHashMap::default();
    while let Some(current) = queue.pop_front() {
        // Reading a file depends on no other, so everything waiting is read at once
        // and in parallel, while the walk itself keeps its order and its answer.
        if !read.contains_key(&current) && !files.knows(&current) {
            let waiting: Vec<&PathBuf> = std::iter::once(&current)
                .chain(&queue)
                .filter(|file| !read.contains_key(*file) && !files.knows(file))
                .collect();
            let found: Vec<Vec<String>> = waiting
                .par_iter()
                .map(|file| imported_specifiers(file, reading).unwrap_or_default())
                .collect();
            read.extend(waiting.into_iter().cloned().zip(found));
        }
        // Read once, for both questions that need them, and only if one does.
        let specifiers = OnceCell::new();
        if let Some(found) = read.remove(&current) {
            let _ = specifiers.set(found);
        }
        let specifiers = || {
            specifiers
                .get_or_init(|| imported_specifiers(&current, reading).unwrap_or_default())
                .as_slice()
        };
        if change.files().contains(&current)
            || change.marks_package(&current)
            || change.repoints(files.resolver(), &current, specifiers)
        {
            let hit = Hit {
                direction: Direction::Downstream,
                rendered: None,
                path: trace(&came_from, &current),
            };
            return Search {
                hit: Some(hit),
                visited: came_from.into_keys().collect(),
            };
        }

        for next in files.edges_from(&current, specifiers).iter() {
            if !came_from.contains_key(next) {
                came_from.insert(next.clone(), Some(current.clone()));
                queue.push_back(next.clone());
            }
        }
    }

    Search {
        hit: None,
        visited: came_from.into_keys().collect(),
    }
}

/// Walks forward from every changed file, looking for an anchor.
///
/// Roots are visited in a stable order so that a run with several changed files
/// always reports the same one.
pub fn upstream(
    anchors: &[PathBuf],
    changed: &AHashSet<PathBuf>,
    files: &FileGraph,
    reading: &Reading,
) -> Search<Hit> {
    let anchor_set: AHashSet<&PathBuf> = anchors.iter().collect();

    let mut roots: Vec<&PathBuf> = changed.iter().collect();
    roots.sort();

    let mut visited = AHashSet::default();
    for root in roots {
        let mut came_from: AHashMap<PathBuf, Option<PathBuf>> = AHashMap::default();
        let mut queue = VecDeque::new();
        came_from.insert(root.clone(), None);
        queue.push_back(root.clone());

        while let Some(current) = queue.pop_front() {
            if anchor_set.contains(&current) {
                let hit = Hit {
                    direction: Direction::Upstream,
                    rendered: None,
                    path: trace(&came_from, &current),
                };
                visited.extend(came_from.into_keys());
                return Search {
                    hit: Some(hit),
                    visited,
                };
            }

            let specifiers = || imported_specifiers(&current, reading).unwrap_or_default();
            for next in files.edges_from(&current, specifiers).iter() {
                if !came_from.contains_key(next) {
                    came_from.insert(next.clone(), Some(current.clone()));
                    queue.push_back(next.clone());
                }
            }
        }
        visited.extend(came_from.into_keys());
    }

    Search { hit: None, visited }
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
pub fn downstream_symbols(anchors: &[PathBuf], marks: &Marks, graph: &Graph) -> Search<Vec<Node>> {
    let mut came_from: AHashMap<Node, Option<Node>> = AHashMap::default();
    let mut queue = VecDeque::new();

    for anchor in anchors {
        let node = Node::File(graph.file_id(anchor));
        if came_from.insert(node, None).is_none() {
            queue.push_back(node);
        }
    }

    let visited = |came_from: &AHashMap<Node, Option<Node>>| {
        let files: AHashSet<_> = came_from.keys().map(Node::file).collect();
        files.into_iter().map(|file| graph.path(file)).collect()
    };
    while let Some(current) = queue.pop_front() {
        if marks.is_marked(graph, current) {
            return Search {
                hit: Some(trace_nodes(&came_from, current)),
                visited: visited(&came_from),
            };
        }

        for next in graph.edges(current) {
            if !came_from.contains_key(&next) {
                came_from.insert(next, Some(current));
                queue.push_back(next);
            }
        }
    }

    Search {
        hit: None,
        visited: visited(&came_from),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Several anchors' searches walk the same files, and reading a file's imports
    /// means parsing it, so a file is read once however often it is walked.
    #[test]
    fn a_file_walked_twice_is_read_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("page.ts"), "import './button';\n").unwrap();
        std::fs::write(root.join("button.ts"), "").unwrap();
        let files = FileGraph::new(Arc::new(Resolver::new(
            Arc::new(crate::config::Configs::new(&root)),
            Arc::default(),
            root.clone(),
            Arc::default(),
            Arc::default(),
            crate::config::Lookup::default(),
        )));

        let page = root.join("page.ts");
        let button = [root.join("button.ts")];
        let first = files.edges_from(&page, || vec!["./button".to_string()]);
        assert_eq!(&*first, &button);
        let again = files.edges_from(&page, || -> Vec<String> {
            panic!("the imports were read again")
        });
        assert_eq!(&*again, &button);
    }
}
