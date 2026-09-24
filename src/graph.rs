//! The node graph.
//!
//! An edge `A -> B` means "A's observable behaviour may depend on B". Modules are
//! analysed the first time a traversal enters them, and cached, so the cost stays
//! proportional to the anchor's reachable graph.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ahash::{AHashMap, AHashSet};

use crate::module::Reading;
use crate::module::{
    DeclId, ExportTarget, ImportRef, ImportTarget, LineTable, Member, ModuleAnalysis, SourceId,
    is_source_file,
};
use crate::resolve::{Resolver, SideEffects};

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u32);

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NameId(pub u32);

/// A node in the dependency graph.
///
/// `File(f)` is the umbrella: it depends on every other node of `f`, and it is the
/// only node for leaves, assets, and any module the analyser gave up on. Attaching an
/// edge to it is how any rule says "I could not be precise about f".
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Node {
    File(FileId),
    Decl(FileId, DeclId),
    Export(FileId, NameId),
    /// One property of an export bound to a plain object literal: `Member(f,
    /// utils, formatDate)`. It depends on that property's value alone, where
    /// `Export(f, utils)` depends on every property.
    Member(FileId, NameId, NameId),
    ModuleInit(FileId),
}

impl Node {
    pub fn file(&self) -> FileId {
        match self {
            Node::File(f)
            | Node::Decl(f, _)
            | Node::Export(f, _)
            | Node::Member(f, _, _)
            | Node::ModuleInit(f) => *f,
        }
    }
}

/// What we know about one file once it has been looked at.
pub struct Analysed {
    pub analysis: ModuleAnalysis,
    pub line_table: LineTable,
    /// Where each import specifier resolves to, by source index.
    pub resolved: Vec<Option<FileId>>,
}

pub struct Graph {
    resolver: Resolver,
    reading: Reading,
    /// Whether this run may treat an import as deferred to its first use.
    ///
    /// One answer for the whole run rather than one per file: it describes the
    /// bundler, and the anchor is what picks one. See [`crate::config`].
    inline_requires: bool,
    paths: RefCell<Vec<PathBuf>>,
    path_ids: RefCell<AHashMap<PathBuf, FileId>>,
    names: RefCell<Vec<String>>,
    name_ids: RefCell<AHashMap<String, NameId>>,
    analyses: RefCell<AHashMap<FileId, Option<Rc<Analysed>>>>,
    /// Names a file used to export and no longer does.
    ///
    /// Empty unless a base revision said so, since a departed name leaves no trace
    /// in the file it left. Keeping them here lets `Export(f, name)` stay a node the
    /// consumers that ask for it still reach, so the mark that says it has gone
    /// reaches exactly those consumers and no others.
    lost_exports: RefCell<AHashMap<FileId, AHashSet<NameId>>>,
}

impl Graph {
    pub fn new(
        reading: Reading,
        inline_requires: bool,
        unresolved: std::sync::Arc<crate::resolve::Unresolved>,
        root: PathBuf,
        packages: std::sync::Arc<crate::lockfile::Changed>,
    ) -> Self {
        Self {
            resolver: Resolver::new(reading.configs.clone(), unresolved, root, packages),
            reading,
            inline_requires,
            paths: RefCell::new(Vec::new()),
            path_ids: RefCell::new(AHashMap::default()),
            names: RefCell::new(Vec::new()),
            name_ids: RefCell::new(AHashMap::default()),
            analyses: RefCell::new(AHashMap::default()),
            lost_exports: RefCell::new(AHashMap::default()),
        }
    }

    pub fn file_id(&self, path: &Path) -> FileId {
        if let Some(id) = self.path_ids.borrow().get(path) {
            return *id;
        }
        let mut paths = self.paths.borrow_mut();
        let id = FileId(paths.len() as u32);
        paths.push(path.to_path_buf());
        self.path_ids.borrow_mut().insert(path.to_path_buf(), id);
        id
    }

    pub fn path(&self, file: FileId) -> PathBuf {
        self.paths.borrow()[file.0 as usize].clone()
    }

    pub fn name_id(&self, name: &str) -> NameId {
        if let Some(id) = self.name_ids.borrow().get(name) {
            return *id;
        }
        let mut names = self.names.borrow_mut();
        let id = NameId(names.len() as u32);
        names.push(name.to_string());
        self.name_ids.borrow_mut().insert(name.to_string(), id);
        id
    }

    pub fn name(&self, name: NameId) -> String {
        self.names.borrow()[name.0 as usize].clone()
    }

    /// Records that `file` no longer exports `name`, and returns the node that says
    /// so.
    pub fn lose_export(&self, file: FileId, name: &str) -> Node {
        let name = self.name_id(name);
        self.lost_exports
            .borrow_mut()
            .entry(file)
            .or_default()
            .insert(name);
        Node::Export(file, name)
    }

    fn has_lost(&self, file: FileId, name: NameId) -> bool {
        self.lost_exports
            .borrow()
            .get(&file)
            .is_some_and(|names| names.contains(&name))
    }

    fn lost(&self, file: FileId) -> Vec<NameId> {
        self.lost_exports
            .borrow()
            .get(&file)
            .map(|names| names.iter().copied().collect())
            .unwrap_or_default()
    }

    /// How this run reads a module, for the parts of the analysis that sit outside
    /// the graph and must read them the same way.
    pub fn reading(&self) -> &Reading {
        &self.reading
    }

    /// The resolver this graph is built on.
    ///
    /// Shared rather than rebuilt by the parts of the run that need one of their own:
    /// resolving the same specifier twice must give the same answer, and a second
    /// resolver would start with an empty cache to prove it.
    pub fn resolver(&self) -> &Resolver {
        &self.resolver
    }

    /// Analyses `file` if it has not been looked at yet. `None` for leaves.
    pub fn analysis(&self, file: FileId) -> Option<Rc<Analysed>> {
        if let Some(cached) = self.analyses.borrow().get(&file) {
            return cached.clone();
        }

        let path = self.path(file);
        let analysed =
            crate::module::analyse(&path, &self.reading).map(|(analysis, line_table)| {
                let resolved = analysis
                    .sources()
                    .iter()
                    .map(|specifier| {
                        self.resolver
                            .resolve(&path, specifier)
                            .map(|target| self.file_id(&target))
                    })
                    .collect();
                Rc::new(Analysed {
                    analysis,
                    line_table,
                    resolved,
                })
            });

        self.analyses.borrow_mut().insert(file, analysed.clone());
        analysed
    }

    fn target_of(&self, analysed: &Analysed, source: SourceId) -> Option<FileId> {
        analysed.resolved.get(source as usize).copied().flatten()
    }

    /// Everything `node` may depend on.
    pub fn edges(&self, node: Node) -> Vec<Node> {
        match node {
            Node::File(file) => self.file_edges(file),
            Node::Decl(file, decl) => self.decl_edges(file, decl),
            Node::Export(file, name) => self.export_edges(file, name),
            Node::Member(file, export, member) => self.member_edges(file, export, member),
            Node::ModuleInit(file) => self.init_edges(file),
        }
    }

    /// `File(f)` depends on every other node of `f`.
    fn file_edges(&self, file: FileId) -> Vec<Node> {
        let Some(analysed) = self.analysis(file) else {
            return Vec::new();
        };

        match &analysed.analysis {
            // A coarse module still has outgoing edges: its import list is
            // extractable even when nothing else is. It reaches each dependency
            // wholesale, which is what the file-level analysis has always done.
            ModuleAnalysis::Coarse { .. } => analysed
                .resolved
                .iter()
                .flatten()
                .map(|target| Node::File(*target))
                .collect(),
            ModuleAnalysis::Fine(module) => {
                let mut edges = Vec::with_capacity(module.decls.len() + module.exports.len() + 1);
                for decl in 0..module.decls.len() {
                    edges.push(Node::Decl(file, decl as DeclId));
                }
                for export in &module.exports {
                    edges.push(Node::Export(file, self.name_id(&export.name)));
                }
                edges.push(Node::ModuleInit(file));
                edges
            }
        }
    }

    fn decl_edges(&self, file: FileId, decl: DeclId) -> Vec<Node> {
        let Some(analysed) = self.analysis(file) else {
            return Vec::new();
        };
        let Some(module) = analysed.analysis.as_fine() else {
            return Vec::new();
        };
        let Some(entry) = module.decls.get(decl as usize) else {
            return Vec::new();
        };

        self.reference_edges(file, &analysed, &entry.refs, &entry.imports)
    }

    /// The nodes a declaration's references, or a member's, point at.
    fn reference_edges(
        &self,
        file: FileId,
        analysed: &Analysed,
        refs: &[DeclId],
        imports: &[ImportRef],
    ) -> Vec<Node> {
        let mut edges = Vec::new();
        for &target in refs {
            edges.push(Node::Decl(file, target));
        }
        for import in imports {
            let Some(target) = self.target_of(analysed, import.source) else {
                continue;
            };
            match &import.target {
                ImportTarget::Named(name) => edges.push(self.resolve_export(target, name)),
                ImportTarget::Member { export, member } => {
                    edges.push(self.resolve_member(target, export, member));
                }
                ImportTarget::Namespace => edges.extend(self.all_exports(target)),
            }
        }
        edges
    }

    fn member_edges(&self, file: FileId, export: NameId, member: NameId) -> Vec<Node> {
        let Some(analysed) = self.analysis(file) else {
            return Vec::new();
        };
        let export = self.name(export);
        let Some(entry) = self.member_of(file, &export, &self.name(member)) else {
            // The object no longer has this shape, so the export as a whole is what
            // was read. Nothing hands out a member node that is not there, but a
            // stale one costs precision rather than an answer.
            return vec![self.resolve_export(file, &export)];
        };
        let mut edges = self.reference_edges(file, &analysed, &entry.refs, &entry.imports);
        // Reading a member reaches the module the way reading the export would.
        if self.inline_requires {
            edges.push(Node::ModuleInit(file));
        }
        edges
    }

    /// The member `member` of the object `file` exports as `export`, if the export
    /// is a declaration of this file whose properties can be read apart.
    fn member_of(&self, file: FileId, export: &str, member: &str) -> Option<Member> {
        let analysed = self.analysis(file)?;
        let module = analysed.analysis.as_fine()?;
        let Some(ExportTarget::Local(decl)) = module.export_named(export).map(|e| &e.target) else {
            return None;
        };
        module
            .decls
            .get(*decl as usize)?
            .members
            .iter()
            .find(|entry| entry.name == member)
            .cloned()
    }

    /// The node a read of `member` off the export `export` of `file` should point
    /// at: the member itself where the export is an object of this file that can be
    /// read apart, and the whole export otherwise — a re-export, a lost name, a
    /// value of any other shape.
    ///
    /// A re-export is not followed: the statement that forwards the name is part of
    /// what the reader depends on, and only `Export` of the forwarding file says so.
    pub fn resolve_member(&self, file: FileId, export: &str, member: &str) -> Node {
        if is_source_file(&self.path(file)) && self.member_of(file, export, member).is_some() {
            return Node::Member(file, self.name_id(export), self.name_id(member));
        }
        self.resolve_export(file, export)
    }

    fn export_edges(&self, file: FileId, name: NameId) -> Vec<Node> {
        let Some(analysed) = self.analysis(file) else {
            return Vec::new();
        };
        let Some(module) = analysed.analysis.as_fine() else {
            return vec![Node::File(file)];
        };

        let text = self.name(name);
        let mut edges = match module.export_named(&text).map(|e| &e.target) {
            Some(ExportTarget::Local(decl)) => vec![Node::Decl(file, *decl)],
            Some(ExportTarget::Reexport { source, name }) => {
                match self.target_of(&analysed, *source) {
                    Some(target) => vec![self.resolve_export(target, name)],
                    None => Vec::new(),
                }
            }
            Some(ExportTarget::ReexportAll { source }) => {
                match self.target_of(&analysed, *source) {
                    Some(target) => self.all_exports(target),
                    None => Vec::new(),
                }
            }
            // Not in the table directly: it may arrive through `export *`.
            None => {
                let mut seen = AHashSet::default();
                match self.through_stars(file, &text, &mut seen) {
                    Some(node) => vec![node],
                    None => vec![Node::File(file)],
                }
            }
        };
        // With imports deferred to first use, this is where a module gets evaluated:
        // not when it is imported, but when something reaches one of its names. Every
        // way of reaching into another module lands on an export node, so stating it
        // here states it once — for a plain import, for a re-export, for a name that
        // arrived through `export *`, and for each name of a namespace.
        if self.inline_requires {
            edges.push(Node::ModuleInit(file));
        }
        edges
    }

    fn init_edges(&self, file: FileId) -> Vec<Node> {
        let Some(analysed) = self.analysis(file) else {
            return Vec::new();
        };
        let Some(module) = analysed.analysis.as_fine() else {
            return vec![Node::File(file)];
        };

        let mut edges = Vec::new();
        // A module that declares itself free of side effects is claiming that these
        // statements do nothing observable. The claim covers its own code only, so
        // the edges below — which belong to the modules it imports — still stand.
        if self.runs_on_import(file) {
            for &decl in &module.init_decls {
                edges.push(Node::Decl(file, decl));
            }
        }
        // Importing a module runs its initialisation, in any form — unless the
        // project defers each import to the first use of the binding it introduces,
        // in which case importing runs nothing and the edge belongs to whoever uses
        // the binding. A *bare* import introduces no binding, so there is nothing to
        // defer and it runs either way.
        for (source, target) in analysed.resolved.iter().enumerate() {
            let Some(target) = *target else { continue };
            let bare = module.bare_sources.contains(&(source as SourceId));
            if self.inline_requires && !bare {
                continue;
            }
            if is_source_file(&self.path(target)) {
                edges.push(Node::ModuleInit(target));
            } else if bare && self.runs_on_import(target) {
                // Only a *bare* import of a non-source file is a module-level side
                // effect: `import "./theme.css"` affects everyone importing this
                // module. A binding import of an asset reaches only the declarations
                // that use the binding, so it is a declaration edge instead. Whether
                // loading the target runs anything is the target's claim to make,
                // not this module's.
                edges.push(Node::File(target));
            }
        }
        edges
    }

    /// Can evaluating `file` run anything of its own, or does it only define
    /// bindings?
    ///
    /// Only module initialisation asks, and only about the file's own statements. An
    /// export is reached by name whatever its package claims, because that is data
    /// flow rather than a side effect of loading.
    fn runs_on_import(&self, file: FileId) -> bool {
        self.resolver.side_effects(&self.path(file)) == SideEffects::Possible
    }

    /// The node a named import of `name` from `file` should point at.
    ///
    /// A name the target no longer exports points at `Export(target, name)`, which is
    /// where the mark saying so is. A name it never exported points at
    /// `File(target)`, since anything in the target could be where it was meant to
    /// come from.
    pub fn resolve_export(&self, file: FileId, name: &str) -> Node {
        if !is_source_file(&self.path(file)) {
            return Node::File(file);
        }
        let Some(analysed) = self.analysis(file) else {
            return Node::File(file);
        };
        let Some(module) = analysed.analysis.as_fine() else {
            return Node::File(file);
        };

        let id = self.name_id(name);
        if module.export_named(name).is_some() || self.has_lost(file, id) {
            return Node::Export(file, id);
        }

        let mut seen = AHashSet::default();
        self.through_stars(file, name, &mut seen)
            .unwrap_or(Node::File(file))
    }

    /// Follows `export * from` chains looking for `name`, with a cycle guard.
    fn through_stars(&self, file: FileId, name: &str, seen: &mut AHashSet<FileId>) -> Option<Node> {
        if !seen.insert(file) {
            return None;
        }
        let analysed = self.analysis(file)?;
        let module = analysed.analysis.as_fine()?;

        for &source in &module.export_stars {
            let Some(target) = self.target_of(&analysed, source) else {
                continue;
            };
            let Some(target_analysis) = self.analysis(target) else {
                continue;
            };
            match target_analysis.analysis.as_fine() {
                Some(target_module) => {
                    let id = self.name_id(name);
                    if target_module.export_named(name).is_some() || self.has_lost(target, id) {
                        return Some(Node::Export(target, id));
                    }
                    if let Some(node) = self.through_stars(target, name, seen) {
                        return Some(node);
                    }
                }
                // A coarse module in the chain means the unknown names could be
                // anything it holds.
                None => return Some(Node::File(target)),
            }
        }
        None
    }

    /// Every export of `file`, for a namespace import.
    fn all_exports(&self, file: FileId) -> Vec<Node> {
        if !is_source_file(&self.path(file)) {
            return vec![Node::File(file)];
        }
        let mut seen = AHashSet::default();
        let mut nodes = Vec::new();
        self.collect_exports(file, &mut seen, &mut nodes);
        // Taking the whole module reaches it whether or not it exports anything, so
        // with imports deferred this is the one reference that cannot be covered by
        // the export nodes it produces: there may be none.
        if self.inline_requires {
            nodes.push(Node::ModuleInit(file));
        }
        nodes
    }

    fn collect_exports(&self, file: FileId, seen: &mut AHashSet<FileId>, out: &mut Vec<Node>) {
        if !seen.insert(file) {
            return;
        }
        let Some(analysed) = self.analysis(file) else {
            out.push(Node::File(file));
            return;
        };
        let Some(module) = analysed.analysis.as_fine() else {
            out.push(Node::File(file));
            return;
        };

        for export in &module.exports {
            out.push(Node::Export(file, self.name_id(&export.name)));
        }
        // A namespace sees the whole table, including the shape of it, so a name that
        // has left is part of what it sees.
        for name in self.lost(file) {
            out.push(Node::Export(file, name));
        }
        for &source in &module.export_stars {
            if let Some(target) = self.target_of(&analysed, source) {
                self.collect_exports(target, seen, out);
            }
        }
    }

    /// How a node reads in an explanation.
    pub fn render(&self, node: Node, root: &Path) -> String {
        let path = display_path(&self.path(node.file()), root);
        match node {
            Node::File(_) => format!("File({})", path),
            Node::Decl(file, decl) => {
                let name = self
                    .analysis(file)
                    .and_then(|a| {
                        a.analysis
                            .as_fine()
                            .and_then(|m| m.decls.get(decl as usize).map(|d| d.name.clone()))
                    })
                    .unwrap_or_else(|| decl.to_string());
                format!("Decl({}, {})", path, name)
            }
            Node::Export(_, name) => format!("Export({}, {})", path, self.name(name)),
            Node::Member(_, export, member) => {
                format!(
                    "Member({}, {}.{})",
                    path,
                    self.name(export),
                    self.name(member)
                )
            }
            Node::ModuleInit(_) => format!("ModuleInit({})", path),
        }
    }
}

impl Default for Graph {
    fn default() -> Self {
        Self::new(
            Reading::default(),
            false,
            std::sync::Arc::new(crate::resolve::Unresolved::default()),
            PathBuf::from("."),
            std::sync::Arc::new(crate::lockfile::Changed::default()),
        )
    }
}

/// Relative to the root where possible, always with forward slashes, so that an
/// explanation reads the same everywhere.
pub fn display_path(path: &Path, root: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
