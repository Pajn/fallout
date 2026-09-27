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

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use ahash::{AHashMap, AHashSet};

use crate::change::{Change, Extent};
use crate::diff::LineRange;
use crate::graph::{FileId, Fine, Graph, Node, View};
use crate::module::compare::Comparison;
use crate::module::{Decl, DeclId, Export, ExportTarget, FineModule, LineTable, SourceId, Span};

/// What the change marks in one graph.
///
/// Most of it is worked out up front, from what the change says of each file it
/// names. What reads an import the change moved is worked out for each file when a
/// search first asks about it: a moved import can be in a file the change never
/// touched, and finding them all would mean resolving every import in the tree.
pub struct Marks<'c> {
    change: &'c Change,
    /// Every node the change marks from the extents of the files it names.
    marked: AHashSet<Node>,
    /// The nodes of each file that read an import the change may have sent
    /// somewhere else. See [`crate::repoint`].
    repointed: RefCell<AHashMap<FileId, Rc<AHashSet<Node>>>>,
}

impl<'c> Marks<'c> {
    pub fn new(graph: &Graph, change: &'c Change) -> Self {
        Self {
            change,
            marked: marked_nodes(graph, change.extents()),
            repointed: RefCell::new(AHashMap::default()),
        }
    }

    /// Whether a search that arrives at `node` has arrived at the change.
    ///
    /// A node standing for a changed package is marked whole. `File(f)` is the
    /// umbrella: marking it says "something in f changed, and we cannot say what".
    /// Every node of `f` is therefore marked with it, or a search that reaches only
    /// a declaration would miss a whole-file change.
    pub fn is_marked(&self, graph: &Graph, node: Node) -> bool {
        let file = node.file();
        if self.change.marks_package(&graph.path(file)) {
            return true;
        }
        let umbrella = Node::File(file);
        self.marked.contains(&node)
            || self.marked.contains(&umbrella)
            || self.repointed(graph, file).contains(&node)
            || self.repointed(graph, file).contains(&umbrella)
    }

    /// The nodes of `file` that read an import the change may have sent to another
    /// file, which are as changed as if the import had been rewritten.
    fn repointed(&self, graph: &Graph, file: FileId) -> Rc<AHashSet<Node>> {
        if let Some(known) = self.repointed.borrow().get(&file) {
            return known.clone();
        }
        let mut nodes = AHashSet::default();
        let moved = graph.moved_sources(file);
        if !moved.is_empty() {
            match graph.view(file) {
                View::Fine(fine) => mark_sources(graph, &fine, &moved, &mut nodes),
                View::Opaque(_) => {
                    nodes.insert(Node::File(file));
                }
            }
        }
        let nodes = Rc::new(nodes);
        self.repointed.borrow_mut().insert(file, nodes.clone());
        nodes
    }
}

/// Every node the extents mark, at declaration granularity.
///
/// A file whose whole extent the change reaches — a binary file, a rename, a path
/// given with `--changed` — marks `File(f)`, which is the file-level behaviour and
/// stays available forever.
fn marked_nodes<'e>(
    graph: &Graph,
    extents: impl IntoIterator<Item = (&'e Path, &'e Extent)>,
) -> AHashSet<Node> {
    let mut marked = AHashSet::default();
    for (path, extent) in extents {
        let id = graph.file_id(path);
        match extent {
            // Nothing observable differs: a reworded comment, a reflowed expression.
            // This is the one case where a file the change names marks nothing.
            Extent::Unchanged => {}
            Extent::Whole => {
                marked.insert(Node::File(id));
            }
            Extent::Lines(ranges) => mark_ranges(graph, id, ranges, &mut marked),
            Extent::Statements(comparison) => mark_statements(graph, id, comparison, &mut marked),
        }
    }
    marked
}

/// Marks what the base version of a file says actually changed in it.
fn mark_statements(graph: &Graph, file: FileId, comparison: &Comparison, out: &mut AHashSet<Node>) {
    // Knowing exactly what changed is no use in a file with no interior to put it in:
    // a consumer of a module the analyser gave up on reaches one node, so that is the
    // node to mark.
    let Some(fine) = graph.view(file).fine() else {
        out.insert(Node::File(file));
        return;
    };
    let module = fine.module();

    // A name that has gone is still a node, and one only the consumers that ask for
    // it arrive at. The graph was built knowing it had gone, which is what keeps it
    // one they can reach.
    for name in &comparison.lost_exports {
        out.insert(Node::Export(file, graph.name_id(name)));
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
}

fn mark_ranges(graph: &Graph, file: FileId, ranges: &[LineRange], out: &mut AHashSet<Node>) {
    // A leaf, or a module the analyser gave up on, has no interior to attribute a
    // line to.
    let Some(fine) = graph.view(file).fine() else {
        out.insert(Node::File(file));
        return;
    };
    let (analysed, module) = (fine.analysed(), fine.module());

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

    // A range intersecting a statement marks every declaration it declares, and
    // the members of an object declaration it touches.
    for (id, decl) in module.decls.iter().enumerate() {
        if decl.span.intersects(start, end) {
            out.insert(Node::Decl(file, id as DeclId));
            hit_statement = true;
            mark_members(graph, file, id as DeclId, decl, start, end, out);
        }
    }

    // A range touching an export statement marks those export nodes.
    for export in &module.exports {
        if export.span.intersects(start, end) {
            out.insert(Node::Export(file, graph.name_id(&export.name)));
            hit_statement = true;
        }
        mark_forwarded(graph, file, module, export, start, end, out);
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

/// Marks the members of an object declaration that a byte range touches.
///
/// An edit inside one property is an edit to that member alone. Anything else in
/// the statement the range touches — the declaration around the literal — is part
/// of every member, because every member is read through it. The separators
/// between properties belong to none of them.
fn mark_members(
    graph: &Graph,
    file: FileId,
    id: DeclId,
    decl: &Decl,
    start: u32,
    end: u32,
    out: &mut AHashSet<Node>,
) {
    let framing = !within(decl.interior, decl.span, start, end);
    for member in &decl.members {
        if framing || member.span.intersects(start, end) {
            out.insert(Node::Member(file, id, graph.name_id(&member.name)));
        }
    }

    // A factory's members depend on the arguments its rule names and on the call
    // around them, and on nothing in the other arguments. An argument the call does
    // not pass is touched by an edit where it would be written, which is where
    // removing it leaves its mark.
    if let Some(call) = &decl.factory
        && let Some(rule) = graph.made_by(file, id)
    {
        let framing = !within(call.interior, decl.span, start, end);
        for (member, args) in rule.members {
            let named = args.iter().any(|&index| match call.args.get(index) {
                Some((span, _)) => span.intersects(start, end),
                None => call.missing.intersects(start, end),
            });
            if framing || named {
                out.insert(Node::Member(file, id, graph.name_id(member)));
            }
        }
    }
}

/// Marks every member of an object that a separate export statement names, when
/// the range touches that statement: which object a name stands for is part of
/// every member read through the name.
fn mark_forwarded(
    graph: &Graph,
    file: FileId,
    module: &FineModule,
    export: &Export,
    start: u32,
    end: u32,
    out: &mut AHashSet<Node>,
) {
    let ExportTarget::Local(id) = export.target else {
        return;
    };
    let Some(decl) = module.decls.get(id as usize) else {
        return;
    };
    if export.span == decl.span || !export.span.intersects(start, end) {
        return;
    }
    for member in &decl.members {
        out.insert(Node::Member(file, id, graph.name_id(&member.name)));
    }
    if decl.factory.is_some()
        && let Some(rule) = graph.made_by(file, id)
    {
        for member in rule.member_names() {
            out.insert(Node::Member(file, id, graph.name_id(member)));
        }
    }
}

/// Whether the part of a range that falls inside `outer` lies wholly inside `inner`.
fn within(inner: Span, outer: Span, start: u32, end: u32) -> bool {
    if start == end {
        return inner.contains(start);
    }
    start.max(outer.start) >= inner.start && end.min(outer.end) <= inner.end
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

/// Marks every node of `file` that reads one of `sources`: what an import that
/// now resolves somewhere else changes, when the import itself is as written.
///
/// Module initialisation evaluates every import, so it is always among them. A
/// factory call that reads one in any argument has every member marked, since
/// which argument is not worth telling apart for a change this rare. An
/// `export *` of one takes the whole file, since which names it brings is exactly
/// what moved.
fn mark_sources(graph: &Graph, fine: &Fine, sources: &[SourceId], out: &mut AHashSet<Node>) {
    let (file, module) = (fine.file(), fine.module());
    let reads = |imports: &[crate::module::ImportRef]| {
        imports
            .iter()
            .any(|import| sources.contains(&import.source))
    };
    out.insert(Node::ModuleInit(file));
    if module
        .export_stars
        .iter()
        .any(|source| sources.contains(source))
    {
        out.insert(Node::File(file));
    }
    for (id, decl) in module.decls.iter().enumerate() {
        let id = id as DeclId;
        if reads(&decl.imports) {
            out.insert(Node::Decl(file, id));
        }
        for member in &decl.members {
            if reads(&member.imports) {
                out.insert(Node::Member(file, id, graph.name_id(&member.name)));
            }
        }
        if let Some(call) = &decl.factory {
            let read = reads(&call.frame.imports)
                || call.args.iter().any(|(_, deps)| reads(&deps.imports));
            if let Some(rule) = graph.made_by(file, id).filter(|_| read) {
                for member in rule.member_names() {
                    out.insert(Node::Member(file, id, graph.name_id(member)));
                }
            }
        }
    }
    for export in &module.exports {
        let forwards = match &export.target {
            ExportTarget::Reexport { source, .. } | ExportTarget::ReexportAll { source } => {
                sources.contains(source)
            }
            ExportTarget::Local(_) => false,
        };
        if forwards {
            out.insert(Node::Export(file, graph.name_id(&export.name)));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::diff::ChangeSet;
    use crate::graph::tests::{graph_of, reading, tree};

    /// Every node `file` has, as its own `File(f)` reaches them.
    fn nodes_of(graph: &Graph, file: FileId) -> Vec<Node> {
        let mut nodes = graph.edges(Node::File(file));
        assert!(!nodes.is_empty());
        nodes.push(Node::File(file));
        nodes
    }

    /// Marking `File(f)` says something in f changed and nothing finer, so a search
    /// that arrives at any part of f has arrived at the change.
    #[test]
    fn every_node_of_a_file_marked_whole_counts_as_marked() {
        let (_dir, root) = tree(&[
            ("page.ts", "export const a = 1;\nexport const b = 2;\n"),
            ("other.ts", "export const c = 3;\n"),
        ]);
        let change = Change::read(
            &root,
            ChangeSet::default(),
            &[PathBuf::from("page.ts")],
            None,
            reading(&root),
        );
        let graph = graph_of(&root, &change);
        let marks = Marks::new(&graph, &change);

        let page = graph.file_id(&root.join("page.ts"));
        for node in nodes_of(&graph, page) {
            assert!(marks.is_marked(&graph, node), "{node:?}");
        }
        let other = graph.file_id(&root.join("other.ts"));
        for node in nodes_of(&graph, other) {
            assert!(!marks.is_marked(&graph, node), "{node:?}");
        }
    }

    /// Marking one file reads only the graph, never what marking another wrote, so
    /// the order the change lists its files in cannot matter.
    #[test]
    fn marks_come_out_the_same_whatever_order_the_extents_are_in() {
        let slice = |kind: &str| {
            format!(
                "import {{ createAsyncThunk }} from './store';\n\
                 export const t = createAsyncThunk('{kind}', async () => 1);\n"
            )
        };
        let (_dir, root) = tree(&[
            (
                "lib.ts",
                "export const renamed = 1;\nexport const kept = 2;\n",
            ),
            (
                "store.ts",
                "export * from './lib';\nexport * from './redux';\n",
            ),
            (
                "redux.ts",
                "import { createAsyncThunk as base } from '@reduxjs/toolkit';\n\
                 export const createAsyncThunk = base.withTypes<{ state: unknown }>();\n",
            ),
            ("slice.ts", &slice("a/now")),
            ("page.ts", "export const p = 1;\n"),
            ("util.ts", "export const a = 1;\n\nexport const b = 2;\n"),
        ]);
        let earlier: AHashMap<PathBuf, String> = [
            (
                root.join("lib.ts"),
                "export const original = 1;\nexport const kept = 2;\n".to_string(),
            ),
            (root.join("slice.ts"), slice("a/then")),
        ]
        .into_iter()
        .collect();
        let change = Change::read(
            &root,
            ChangeSet {
                files: vec![crate::diff::ChangedFile {
                    path: PathBuf::from("util.ts"),
                    change: crate::diff::FileChange::Modified {
                        ranges: vec![LineRange { start: 3, len: 1 }],
                    },
                }],
                ..Default::default()
            },
            &[
                PathBuf::from("lib.ts"),
                PathBuf::from("slice.ts"),
                PathBuf::from("page.ts"),
            ],
            Some(Box::new(earlier)),
            reading(&root),
        );
        let extents: Vec<(&Path, &Extent)> = change.extents().collect();
        assert_eq!(extents.len(), 4);

        // A graph of its own for each order, so that nothing one order analysed
        // is there for the next to find.
        let marks = |order: &[usize]| -> Vec<String> {
            let graph = graph_of(&root, &change);
            let marked = marked_nodes(&graph, order.iter().map(|&index| extents[index]));
            let mut rendered: Vec<String> = marked
                .into_iter()
                .map(|node| graph.render(node, &root))
                .collect();
            rendered.sort();
            rendered
        };
        let expected = marks(&[0, 1, 2, 3]);
        assert!(expected.contains(&"Export(lib.ts, original)".to_string()));
        assert!(expected.contains(&"Member(slice.ts, t.pending)".to_string()));
        assert!(expected.contains(&"File(page.ts)".to_string()));
        for order in [[3, 2, 1, 0], [1, 3, 0, 2], [2, 0, 3, 1]] {
            assert_eq!(marks(&order), expected, "{order:?}");
        }
    }

    /// An `export *` whose module moved brings a set of names nobody can list, so
    /// the whole file is marked, and with it every part of the file.
    #[test]
    fn every_node_of_a_file_repointed_whole_counts_as_marked() {
        let (_dir, root) = tree(&[(
            "barrel.ts",
            "export * from './shim';\nexport const b = 1;\n",
        )]);
        // Named and not there, so deleted: `./shim` resolved to it before.
        let change = Change::read(
            &root,
            ChangeSet::default(),
            &[PathBuf::from("shim.ts")],
            None,
            reading(&root),
        );
        let graph = graph_of(&root, &change);
        let marks = Marks::new(&graph, &change);

        let barrel = graph.file_id(&root.join("barrel.ts"));
        for node in nodes_of(&graph, barrel) {
            assert!(marks.is_marked(&graph, node), "{node:?}");
        }
    }
}
