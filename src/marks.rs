//! Attributing a change to graph nodes.
//!
//! Analysis decides *granularity*; the diff decides *change*. Keeping the two apart
//! is what makes each refinement safe to add on its own: an unresolvable import is
//! not "changed", it is "coarse".

use ahash::AHashSet;

use crate::diff::{ChangeSet, FileChange, LineRange};
use crate::graph::{Graph, Node};
use crate::module::{DeclId, LineTable, ModuleAnalysis, Span};

/// Every node a change set marks, at declaration granularity.
///
/// A file the diff describes without line information — a binary file, a rename, or
/// a path given with `--changed` — marks `File(f)`, which is the file-level
/// behaviour and stays available forever.
pub fn marked_nodes(
    graph: &Graph,
    diff: &ChangeSet,
    root: &std::path::Path,
    explicit: &[std::path::PathBuf],
) -> AHashSet<Node> {
    let mut marked = AHashSet::default();

    for file in &diff.files {
        if file.change == FileChange::Deleted {
            continue;
        }
        let Ok(path) = root.join(&file.path).canonicalize() else {
            continue;
        };
        let id = graph.file_id(&path);

        match &file.change {
            FileChange::Modified { ranges } => {
                mark_ranges(graph, id, ranges, &mut marked);
            }
            // No line information: the whole file.
            FileChange::Opaque => {
                marked.insert(Node::File(id));
            }
            FileChange::Deleted => unreachable!("dropped above"),
        }
    }

    for path in explicit {
        if let Ok(path) = path.canonicalize() {
            marked.insert(Node::File(graph.file_id(&path)));
        }
    }

    marked
}

fn mark_ranges(
    graph: &Graph,
    file: crate::graph::FileId,
    ranges: &[LineRange],
    out: &mut AHashSet<Node>,
) {
    let Some(analysed) = graph.analysis(file) else {
        // A leaf has no interior to attribute a line to.
        out.insert(Node::File(file));
        return;
    };
    let Some(module) = analysed.analysis.as_fine() else {
        out.insert(Node::File(file));
        return;
    };

    for range in ranges {
        let (start, end) = byte_range(&analysed.line_table, *range);

        let mut hit_statement = false;

        // A range intersecting a statement marks every declaration it declares.
        for (id, decl) in module.decls.iter().enumerate() {
            if decl.span.intersects(start, end) {
                out.insert(Node::Decl(file, id as DeclId));
                hit_statement = true;
            }
        }

        // A range touching an export statement marks those export nodes.
        for export in &module.exports {
            if export.span.intersects(start, end) {
                out.insert(Node::Export(file, graph.name_id(&export.name)));
                hit_statement = true;
            }
        }

        // A range touching an import statement marks every declaration referencing
        // the bindings it introduced, and module initialisation with it.
        for (span, users) in &module.import_spans {
            if span.intersects(start, end) {
                for &user in users {
                    out.insert(Node::Decl(file, user));
                }
                out.insert(Node::ModuleInit(file));
                hit_statement = true;
            }
        }

        if hit_statement {
            continue;
        }

        // The range fell in a gap between statements. A deleted statement may have
        // been an impure one we can no longer see, so both neighbours are marked
        // along with module initialisation.
        mark_gap(file, module_spans(module).as_slice(), start, end, out);
        out.insert(Node::ModuleInit(file));
    }
}

fn module_spans(module: &crate::module::FineModule) -> Vec<(Span, DeclId)> {
    let mut spans: Vec<(Span, DeclId)> = module
        .decls
        .iter()
        .enumerate()
        .map(|(id, decl)| (decl.span, id as DeclId))
        .collect();
    spans.sort_by_key(|(span, _)| *span);
    spans
}

fn mark_gap(
    file: crate::graph::FileId,
    spans: &[(Span, DeclId)],
    start: u32,
    end: u32,
    out: &mut AHashSet<Node>,
) {
    let before = spans.iter().rev().find(|(span, _)| span.end <= start);
    let after = spans.iter().find(|(span, _)| span.start >= end);

    for (_, decl) in before.into_iter().chain(after) {
        out.insert(Node::Decl(file, *decl));
    }
}

/// A line range as byte offsets. A zero-length range is a deletion, kept zero-width
/// at the start of the line the removed content used to occupy, so that it falls
/// inside the statement it was part of rather than past the end of it.
fn byte_range(table: &LineTable, range: LineRange) -> (u32, u32) {
    if range.len == 0 {
        let at = table.line_start(range.start);
        return (at, at);
    }
    let start = table.line_start(range.start);
    let end = table.line_end(range.start + range.len - 1);
    (start, end.max(start))
}

/// Whether a file's analysis can support declaration-level marks at all.
pub fn is_fine(graph: &Graph, file: crate::graph::FileId) -> bool {
    graph
        .analysis(file)
        .is_some_and(|a| matches!(a.analysis, ModuleAnalysis::Fine(_)))
}
