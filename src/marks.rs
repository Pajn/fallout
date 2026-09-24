//! Attributing a change to graph nodes.
//!
//! Analysis decides *granularity*; the diff decides *change*. Keeping the two apart
//! is what makes each refinement safe to add on its own: an unresolvable import is
//! not "changed", it is "coarse".
//!
//! A change can be described two ways. Given a base revision, each top-level
//! statement is compared against its counterpart there, which says exactly what
//! differs. Without one, the diff's line ranges are laid over the statement spans,
//! which says roughly what differs and guesses at the rest.

use std::path::{Path, PathBuf};

use ahash::AHashSet;

use crate::base::Base;
use crate::diff::{ChangeSet, FileChange, LineRange};
use crate::graph::{FileId, Graph, Node};
use crate::module::{DeclId, FineModule, LineTable, ModuleAnalysis, Span};

/// Every node a change set marks, at declaration granularity.
///
/// A base revision, where there is one, decides what changed in each file; the
/// diff's line ranges are the fallback for the rest.
///
/// A file described without line information — a binary file, a rename, or a path
/// given with `--changed` — marks `File(f)`, which is the file-level behaviour and
/// stays available forever.
pub fn marked_nodes(
    graph: &Graph,
    diff: &ChangeSet,
    root: &Path,
    explicit: &[PathBuf],
    base: Option<&Base>,
) -> AHashSet<Node> {
    let mut marked = AHashSet::default();

    for file in &diff.files {
        if file.change == FileChange::Deleted {
            continue;
        }
        let Ok(path) = dunce::canonicalize(root.join(&file.path)) else {
            continue;
        };
        let id = graph.file_id(&path);

        if mark_against_base(graph, id, &path, base, &mut marked) {
            continue;
        }

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
        let Ok(path) = dunce::canonicalize(path) else {
            continue;
        };
        let id = graph.file_id(&path);
        if !mark_against_base(graph, id, &path, base, &mut marked) {
            marked.insert(Node::File(id));
        }
    }

    marked
}

/// Marks what the base version of a file says actually changed in it.
///
/// `false` means there was no answer to be had — no base revision, no version of
/// this file in it, or two versions that cannot be compared — and the caller falls
/// back on what the diff says.
fn mark_against_base(
    graph: &Graph,
    file: FileId,
    path: &Path,
    base: Option<&Base>,
    out: &mut AHashSet<Node>,
) -> bool {
    let Some(comparison) = base.and_then(|base| base.comparison(path)) else {
        return false;
    };

    // Nothing observable differs: a reworded comment, a reflowed expression. This is
    // the one place the analysis may mark nothing at all for a file the diff names.
    if comparison.is_empty() {
        return true;
    }

    // Knowing exactly what changed is no use in a file with no interior to put it in:
    // a consumer of a module the analyser gave up on reaches one node, so that is the
    // node to mark.
    let Some(module) = graph.analysis(file) else {
        out.insert(Node::File(file));
        return true;
    };
    let Some(module) = module.analysis.as_fine() else {
        out.insert(Node::File(file));
        return true;
    };

    if comparison.whole_file {
        out.insert(Node::File(file));
        return true;
    }

    // A name that has gone is still a node, and one only the consumers that ask for
    // it arrive at.
    for name in &comparison.lost_exports {
        out.insert(graph.lose_export(file, name));
    }

    if comparison.init_differs {
        out.insert(Node::ModuleInit(file));
    }

    for span in &comparison.changed {
        if !attribute(graph, file, module, span.start, span.end, out) {
            // The statement declares nothing and exports nothing, so running the
            // module is all it can affect. No neighbour needs guessing at here: the
            // statement is known, not inferred from the lines around it.
            out.insert(Node::ModuleInit(file));
        }
    }
    true
}

fn mark_ranges(graph: &Graph, file: FileId, ranges: &[LineRange], out: &mut AHashSet<Node>) {
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
        // Nothing on these lines runs, so whatever changed on them was a type. Only
        // a run that asked for types to be ignored ever says this.
        if analysed.line_table.runs_nothing(range.start, range.len) {
            continue;
        }

        let (start, end) = byte_range(&analysed.line_table, *range);

        if attribute(graph, file, module, start, end, out) {
            continue;
        }

        // The range fell in a gap between statements. A deleted statement may have
        // been an impure one we can no longer see, so both neighbours are marked
        // along with module initialisation. A base revision removes the need to
        // guess: see `mark_against_base`.
        mark_gap(file, module_spans(module).as_slice(), start, end, out);
        out.insert(Node::ModuleInit(file));
    }
}

/// Marks every node a byte range touches, and reports whether it touched anything.
fn attribute(
    graph: &Graph,
    file: FileId,
    module: &FineModule,
    start: u32,
    end: u32,
    out: &mut AHashSet<Node>,
) -> bool {
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

    hit_statement
}

fn module_spans(module: &FineModule) -> Vec<(Span, DeclId)> {
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
    file: FileId,
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
pub fn is_fine(graph: &Graph, file: FileId) -> bool {
    graph
        .analysis(file)
        .is_some_and(|a| matches!(a.analysis, ModuleAnalysis::Fine(_)))
}
