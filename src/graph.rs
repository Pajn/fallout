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
    DeclId, ExportTarget, FineModule, ImportRef, ImportTarget, LineTable, Member, ModuleAnalysis,
    SourceId, is_source_file,
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
    /// One property of a declaration bound to a plain object literal: `Member(f,
    /// utils, formatDate)`. It depends on that property's value alone, where
    /// `Decl(f, utils)` depends on every property. Readers in `f` and in the modules
    /// importing the object reach the same node.
    Member(FileId, DeclId, NameId),
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
    /// The files each import specifier resolves to, by source index: none, one, or
    /// for a Sass URL that several files answer, each of them. Only a stylesheet
    /// has a Sass URL, and a stylesheet is never fine, so a fine module's imports
    /// resolve to one file at most.
    pub resolved: Vec<Box<[FileId]>>,
}

/// How far the analysis sees into one file, which every rule asks before any other
/// question about it.
pub enum View {
    Fine(Fine),
    /// A leaf, an asset, or a module the analyser gave up on. Nothing finer than the
    /// file can be said about any of them, so the view does not tell them apart. It
    /// keeps the analysis where there is one, because a coarse module's import list
    /// can still be read.
    Opaque(Option<Rc<Analysed>>),
}

impl View {
    pub fn fine(self) -> Option<Fine> {
        match self {
            View::Fine(fine) => Some(fine),
            View::Opaque(_) => None,
        }
    }
}

/// A file the analysis can see into. Only [`Graph::view`] makes one, and only of a
/// file analysed finely, so what reads it never has to ask again.
pub struct Fine {
    file: FileId,
    analysed: Rc<Analysed>,
}

impl Fine {
    pub fn file(&self) -> FileId {
        self.file
    }

    pub fn analysed(&self) -> &Analysed {
        &self.analysed
    }

    pub fn module(&self) -> &FineModule {
        self.analysed
            .analysis
            .as_fine()
            .expect("a fine view holds a fine analysis")
    }
}

pub struct Graph {
    /// Shared with the file graph the upstream search walks. See [`Graph::file_graph`].
    resolver: std::sync::Arc<Resolver>,
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
    /// Names each file used to export and no longer does, by path. See
    /// [`crate::change::Change::lost_exports`].
    ///
    /// Empty unless a base revision said so, since a departed name leaves no trace
    /// in the file it left. Knowing them lets `Export(f, name)` stay a node the
    /// consumers that ask for it still reach, so the mark that says it has gone
    /// reaches exactly those consumers and no others.
    lost_exports: AHashMap<PathBuf, Vec<String>>,
}

impl Graph {
    pub fn new(
        reading: Reading,
        bundler: crate::config::Bundler,
        unresolved: std::sync::Arc<crate::resolve::Unresolved>,
        root: PathBuf,
        packages: std::sync::Arc<crate::lockfile::Changed>,
        repointing: std::sync::Arc<crate::repoint::Repointing>,
        lost_exports: AHashMap<PathBuf, Vec<String>>,
    ) -> Self {
        Self {
            resolver: std::sync::Arc::new(Resolver::new(
                reading.configs.clone(),
                unresolved,
                root,
                packages,
                repointing,
                bundler.lookup,
            )),
            reading,
            inline_requires: bundler.inline_requires,
            paths: RefCell::new(Vec::new()),
            path_ids: RefCell::new(AHashMap::default()),
            names: RefCell::new(Vec::new()),
            name_ids: RefCell::new(AHashMap::default()),
            analyses: RefCell::new(AHashMap::default()),
            lost_exports,
        }
    }

    /// The imports of `file` the change may have sent to another file, as this
    /// graph's resolver resolves them. None for a file with no analysis, which
    /// imports nothing.
    pub fn moved_sources(&self, file: FileId) -> Vec<SourceId> {
        let Some(analysed) = self.analysis(file) else {
            return Vec::new();
        };
        self.resolver
            .moved(&self.path(file), || analysed.analysis.sources())
            .iter()
            .map(|&source| source as SourceId)
            .collect()
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

    fn has_lost(&self, file: FileId, name: NameId) -> bool {
        self.lost(file).contains(&name)
    }

    /// The names `file` no longer exports, interned when first asked about.
    fn lost(&self, file: FileId) -> Vec<NameId> {
        self.lost_exports
            .get(&self.path(file))
            .map(|names| names.iter().map(|name| self.name_id(name)).collect())
            .unwrap_or_default()
    }

    /// How this run reads a module, for the parts of the analysis that sit outside
    /// the graph and must read them the same way.
    pub fn reading(&self) -> &Reading {
        &self.reading
    }

    /// Whether this run may treat an import as deferred to its first use.
    pub fn inline_requires(&self) -> bool {
        self.inline_requires
    }

    /// The resolver this graph is built on.
    ///
    /// Shared rather than rebuilt by the parts of the run that need one of their own:
    /// resolving the same specifier twice must give the same answer, and a second
    /// resolver would start with an empty cache to prove it.
    pub fn resolver(&self) -> &Resolver {
        &self.resolver
    }

    /// The imports file by file, over this graph's resolver, for the searches that
    /// stay at file granularity. They share its answers for the same reason.
    pub fn file_graph(&self) -> crate::query::FileGraph {
        crate::query::FileGraph::new(self.resolver.clone())
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
                            .iter()
                            .map(|target| self.file_id(target))
                            .collect()
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

    /// How far the analysis sees into `file`, analysing it if it has not been looked
    /// at yet.
    pub fn view(&self, file: FileId) -> View {
        match self.analysis(file) {
            Some(analysed) if analysed.analysis.as_fine().is_some() => {
                View::Fine(Fine { file, analysed })
            }
            analysed => View::Opaque(analysed),
        }
    }

    /// The fine view of `file` for looking a name up in it, if it has one.
    ///
    /// Only a source file can be fine, and any other is not parsed to find that out.
    /// A search parses a file when it arrives at it, which is when the imports the
    /// file makes are resolved and the ones that resolve nowhere are recorded; a
    /// file it only names, and then stops short of, stays out of that record.
    fn lookup(&self, file: FileId) -> Option<Fine> {
        if !is_source_file(&self.path(file)) {
            return None;
        }
        self.view(file).fine()
    }

    pub(crate) fn target_of(&self, analysed: &Analysed, source: SourceId) -> Option<FileId> {
        analysed.resolved.get(source as usize)?.first().copied()
    }

    /// Everything `node` may depend on.
    ///
    /// Whether the file is opaque is decided here, once. Every node of an opaque file
    /// other than `File(f)` stands for the whole of it, which is all anything can say
    /// about it, and `File(f)` reaches each import wholesale, which is what the
    /// file-level analysis has always done. The rules below see only fine files.
    pub fn edges(&self, node: Node) -> Vec<Node> {
        let file = node.file();
        let fine = match self.view(file) {
            View::Fine(fine) => fine,
            View::Opaque(analysed) => {
                return match node {
                    Node::File(_) => analysed
                        .iter()
                        .flat_map(|analysed| {
                            analysed.resolved.iter().flat_map(|targets| targets.iter())
                        })
                        .map(|target| Node::File(*target))
                        .collect(),
                    _ => vec![Node::File(file)],
                };
            }
        };
        match node {
            Node::File(_) => self.file_edges(&fine),
            Node::Decl(_, decl) => self.decl_edges(&fine, decl),
            Node::Export(_, name) => self.export_edges(&fine, name),
            Node::Member(_, decl, member) => self.member_edges(&fine, decl, member),
            Node::ModuleInit(_) => self.init_edges(&fine),
        }
    }

    /// `File(f)` depends on every other node of `f`.
    fn file_edges(&self, fine: &Fine) -> Vec<Node> {
        let (file, module) = (fine.file(), fine.module());
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

    fn decl_edges(&self, fine: &Fine, decl: DeclId) -> Vec<Node> {
        let module = fine.module();
        let Some(entry) = module.decls.get(decl as usize) else {
            return Vec::new();
        };

        self.reference_edges(
            fine,
            Some(decl),
            &entry.refs,
            &entry.member_refs,
            &entry.imports,
        )
    }

    /// The nodes a declaration's references, or a member's, point at: those of
    /// `from`, or of part of it.
    ///
    /// An edge the shared-state rule gave `from` only for calls that read a value a
    /// factory made is left out where the graph confirms that factory; see
    /// [`Graph::only_read_calls`].
    fn reference_edges(
        &self,
        fine: &Fine,
        from: Option<DeclId>,
        refs: &[DeclId],
        member_refs: &[(DeclId, String)],
        imports: &[ImportRef],
    ) -> Vec<Node> {
        let (file, analysed) = (fine.file(), fine.analysed());
        let read_call_edges = from
            .and_then(|from| fine.module().decls.get(from as usize))
            .map_or(&[][..], |entry| entry.read_call_edges.as_slice());
        let mut edges = Vec::new();
        for &target in refs {
            let dropped = read_call_edges.iter().any(|(writer, owners)| {
                *writer == target
                    && owners
                        .iter()
                        .all(|&owner| self.only_read_calls(fine, owner, target))
            });
            if !dropped {
                edges.push(Node::Decl(file, target));
            }
        }
        for (target, member) in member_refs {
            edges.push(self.local_member(fine, *target, member));
        }
        for import in imports {
            let Some(target) = self.target_of(analysed, import.source) else {
                continue;
            };
            match &import.target {
                ImportTarget::Named(name) => edges.extend(self.resolve_export(target, name)),
                ImportTarget::Member { export, member } => {
                    let nodes = self.resolve_member(target, export, member);
                    // Reading a member reaches the module the way reading the export
                    // would, and only an export node says so by itself.
                    if self.inline_requires && nodes.iter().any(|n| matches!(n, Node::Member(..))) {
                        edges.push(Node::ModuleInit(target));
                    }
                    edges.extend(nodes);
                }
                ImportTarget::Namespace => edges.extend(self.all_exports(target)),
            }
        }
        edges
    }

    fn member_edges(&self, fine: &Fine, decl: DeclId, member: NameId) -> Vec<Node> {
        let (file, module) = (fine.file(), fine.module());
        let member = self.name(member);
        let mut edges = match member_of(module, decl, &member) {
            None if let Some(deps) = self.factory_member(fine, decl, &member) => self
                .reference_edges(
                    fine,
                    Some(decl),
                    &deps.refs,
                    &deps.member_refs,
                    &deps.imports,
                ),
            Some(entry) => {
                let mut edges = self.reference_edges(
                    fine,
                    Some(decl),
                    &entry.refs,
                    &entry.member_refs,
                    &entry.imports,
                );
                // Called through the object, it may read any other property as
                // `this`.
                if entry.receiver {
                    edges.push(Node::Decl(file, decl));
                }
                edges
            }
            // Nothing hands out a member node that is not there, but a stale one
            // costs precision rather than an answer.
            None => vec![Node::Decl(file, decl)],
        };
        edges.extend(self.writer_edges(fine, decl, Some(&member)));
        edges
    }

    /// The declarations of `file` that write what `decl` binds, as a read of it from
    /// another module reaches them: every writer, or for a read of one `member`, the
    /// writers of that property and of the whole value.
    ///
    /// They come after every other edge of the node they hang on, so that a path that
    /// reached the change without them is still the one explained.
    fn writer_edges(&self, fine: &Fine, decl: DeclId, member: Option<&str>) -> Vec<Node> {
        let Some(entry) = fine.module().decls.get(decl as usize) else {
            return Vec::new();
        };
        let mut edges: Vec<Node> = entry
            .writers
            .iter()
            .filter(|(_, written)| match (member, written) {
                (Some(read), Some(written)) => read == written,
                _ => true,
            })
            .filter(|(writer, _)| !self.only_read_calls(fine, decl, *writer))
            .map(|(writer, _)| Node::Decl(fine.file(), *writer))
            .collect();
        // Sorted by writer, so one that writes several properties is next to itself.
        edges.dedup();
        edges
    }

    /// Whether `writer` changes what `owner` binds only by calling methods on it that
    /// the factory which made it declares reads: `store.getState().count`, where
    /// `store` is a Zustand store.
    ///
    /// Such a call counts as a write where it is parsed, since only the graph can
    /// tell what made the value. Where it cannot, because the factory is the app's
    /// own, another library's, or none, the call stays a write.
    fn only_read_calls(&self, fine: &Fine, owner: DeclId, writer: DeclId) -> bool {
        let Some(entry) = fine.module().decls.get(owner as usize) else {
            return false;
        };
        let Some((_, methods)) = entry.read_calls.iter().find(|(w, _)| *w == writer) else {
            return false;
        };
        self.made_by(fine.file(), owner)
            .is_some_and(|made| methods.iter().all(|method| made.read_by(method)))
    }

    /// A read of `member` off the declaration `decl` of the same file: that member
    /// where the object has it, and the whole declaration otherwise.
    fn local_member(&self, fine: &Fine, decl: DeclId, member: &str) -> Node {
        if self.has_member(fine, decl, member) {
            Node::Member(fine.file(), decl, self.name_id(member))
        } else {
            Node::Decl(fine.file(), decl)
        }
    }

    /// Whether `member` of `decl` can be read on its own: a property of an object
    /// literal, or one a known factory's rule lists.
    fn has_member(&self, fine: &Fine, decl: DeclId, member: &str) -> bool {
        member_of(fine.module(), decl, member).is_some()
            || self.factory_member(fine, decl, member).is_some()
    }

    /// What `member` of a factory call's result depends on, where the callee is a
    /// factory with a rule that lists it. See [`crate::factories`].
    fn factory_member(
        &self,
        fine: &Fine,
        decl: DeclId,
        member: &str,
    ) -> Option<crate::module::Deps> {
        self.made_by(fine.file(), decl)?.member(member)
    }

    /// The nodes a read of `member` off the export `export` of `file` should point
    /// at: the member itself where the export is an object of this file that can be
    /// read apart, and the whole export otherwise — a re-export, a lost name, a
    /// value of any other shape.
    ///
    /// A re-export is not followed: the statement that forwards the name is part of
    /// what the reader depends on, and only `Export` of the forwarding file says so.
    pub fn resolve_member(&self, file: FileId, export: &str, member: &str) -> Vec<Node> {
        if let Some(fine) = self.lookup(file)
            && let Some(ExportTarget::Local(decl)) =
                fine.module().export_named(export).map(|e| &e.target)
            && self.has_member(&fine, *decl, member)
        {
            return vec![Node::Member(file, *decl, self.name_id(member))];
        }
        self.resolve_export(file, export)
    }

    fn export_edges(&self, fine: &Fine, name: NameId) -> Vec<Node> {
        let (file, analysed, module) = (fine.file(), fine.analysed(), fine.module());
        let text = self.name(name);
        let target = module.export_named(&text).map(|e| &e.target);
        let mut edges = match target {
            Some(ExportTarget::Local(decl)) => vec![Node::Decl(file, *decl)],
            Some(ExportTarget::Reexport { source, name }) => {
                match self.target_of(analysed, *source) {
                    Some(target) => self.resolve_export(target, name),
                    None => Vec::new(),
                }
            }
            Some(ExportTarget::ReexportAll { source }) => match self.target_of(analysed, *source) {
                Some(target) => self.all_exports(target),
                None => Vec::new(),
            },
            // Not in the table directly: it may arrive through `export *`.
            None => {
                let providers = self.star_providers(fine, &text);
                if providers.is_empty() {
                    vec![Node::File(file)]
                } else {
                    providers
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
        // A value exported from where it is declared can be changed by a writer there
        // that the importer never names. Exporting it is no use of it, so no edge of
        // its declaration leads to them; reading it through this name does. A
        // re-export reaches them through the export node of the file that declares
        // the value.
        if let Some(ExportTarget::Local(decl)) = target {
            edges.extend(self.writer_edges(fine, *decl, None));
        }
        edges
    }

    fn init_edges(&self, fine: &Fine) -> Vec<Node> {
        let (file, analysed, module) = (fine.file(), fine.analysed(), fine.module());
        let mut edges = Vec::new();
        // A module that declares itself free of side effects is claiming that these
        // statements do nothing observable. The claim covers its own code only, so
        // the edges below — which belong to the modules it imports — still stand.
        if self.runs_on_import(file) {
            for &decl in &module.init_decls {
                edges.push(Node::Decl(file, decl));
            }
            // A call that may be a factory's runs something unless the callee is a
            // factory that only builds values. Even then it runs the callee, and what
            // makes the callee one is initialisation's to reach: an edit that turns a
            // wrapper with an effect into the factory changes what loading this module
            // does. The call's arguments are what a factory whose creation reaches only
            // the frame leaves alone, and so is a function it calls that is proven to
            // run nothing there. With imports deferred to first use, such a function
            // must read no import either, since reading one evaluates its module. A
            // name that is `withTypes` of a factory reads nothing but it, so it is
            // reached whole.
            for &decl in &module.conditional_init {
                match self.made_by(file, decl) {
                    Some(made) if made.creates_quietly(self.inline_requires) => {
                        let frame = made.frame();
                        edges.extend(self.reference_edges(
                            fine,
                            Some(decl),
                            &frame.refs,
                            &frame.member_refs,
                            &frame.imports,
                        ));
                    }
                    _ => edges.push(Node::Decl(file, decl)),
                }
            }
        }
        // A statement that declares nothing runs for its effect, and what it reads of
        // an import is part of that: `document.title = String(base)` shows `base`.
        // Such a statement has no node of its own, so initialisation reaches the
        // exports it reads, with or without deferred imports. A declaration needs no
        // such edge, since what it reads is reached through its own node wherever it
        // is initialisation or read.
        //
        // With imports deferred to first use, reading an imported binding is also what
        // evaluates the module it names. A declaration whose initialiser reads one as
        // it runs at load evaluates that module as this one is evaluated, so it is
        // initialisation, whole: an edit that makes it read another import changes
        // what loading this module evaluates, as it does for a store's creator. Its
        // reads are followed as any are, through a barrel to the module that provides
        // the name. Evaluating the imported module is that module's doing rather than
        // this one's, so a claim that this module has no side effects leaves these
        // edges standing then, as it leaves the imports below.
        if self.runs_on_import(file) || self.inline_requires {
            edges.extend(self.reference_edges(fine, None, &[], &[], &module.init_imports));
        }
        if self.inline_requires {
            edges.extend(
                module
                    .reads_on_load
                    .iter()
                    .map(|&decl| Node::Decl(file, decl)),
            );
        }
        // Importing a module runs its initialisation, in any form — unless the
        // project defers each import to the first use of the binding it introduces,
        // in which case importing runs nothing and the edge belongs to whoever uses
        // the binding. A *bare* import introduces no binding, so there is nothing to
        // defer and it runs either way.
        for (source, targets) in analysed.resolved.iter().enumerate() {
            let Some(&target) = targets.first() else {
                continue;
            };
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

    /// The nodes a named import of `name` from `file` should point at.
    ///
    /// A name the target no longer exports points at `Export(target, name)`, which is
    /// where the mark saying so is. A name it never exported points at
    /// `File(target)`, since anything in the target could be where it was meant to
    /// come from, and at the file behind each of its `export *` statements for the
    /// same reason: `File(target)` reaches the target's own nodes, not theirs.
    pub fn resolve_export(&self, file: FileId, name: &str) -> Vec<Node> {
        let Some(fine) = self.lookup(file) else {
            return vec![opaque_name(file)];
        };

        let id = self.name_id(name);
        if fine.module().export_named(name).is_some() || self.has_lost(file, id) {
            return vec![Node::Export(file, id)];
        }

        // A name that may arrive through `export *` is still read through this
        // module, which evaluates it when imports are deferred to first use. Its own
        // export node says so, and goes on to each star that could provide it.
        let mut seen = AHashSet::default();
        if self.could_provide(file, name, &mut seen) {
            return vec![Node::Export(file, id)];
        }
        let mut seen = AHashSet::default();
        let mut nodes = Vec::new();
        self.behind_stars(file, &mut seen, &mut nodes);
        nodes
    }

    /// `File` of `file` and of every module behind its `export *` statements, one
    /// star after another, with a cycle guard. Only a fine file has stars to follow.
    fn behind_stars(&self, file: FileId, seen: &mut AHashSet<FileId>, out: &mut Vec<Node>) {
        if !seen.insert(file) {
            return;
        }
        out.push(Node::File(file));
        let View::Fine(fine) = self.view(file) else {
            return;
        };
        for &source in &fine.module().export_stars {
            if let Some(target) = self.target_of(fine.analysed(), source) {
                self.behind_stars(target, seen, out);
            }
        }
    }

    /// The modules behind the `export *` statements of a fine file that could
    /// provide `name`, each as the node a reader of the name goes on to.
    ///
    /// Every one is kept, not the first: a star that could hold any name does not
    /// say the name is there, so a later star that exports it may be where it comes
    /// from. A module the analysis sees inside is `Export(target, name)`, whether it
    /// exports the name itself or forwards it through stars of its own, so that each
    /// barrel on the way stays on the path and is evaluated there when imports are
    /// deferred. A module it cannot see inside is what [`opaque_name`] says.
    ///
    /// A star whose path names no file has no node to go on to. It still makes the
    /// barrel one that could provide the name, so the barrel's own export node is on
    /// the path, and that node is marked when the barrel is.
    pub(crate) fn star_providers(&self, fine: &Fine, name: &str) -> Vec<Node> {
        let id = self.name_id(name);
        let mut providers = Vec::new();
        for &source in &fine.module().export_stars {
            let Some(target) = self.target_of(fine.analysed(), source) else {
                continue;
            };
            // Each star is asked on its own, so that a module one of them has
            // already walked through is not hidden from the next.
            let mut seen = AHashSet::default();
            seen.insert(fine.file());
            if !self.could_provide(target, name, &mut seen) {
                continue;
            }
            providers.push(match self.view(target) {
                View::Fine(_) => Node::Export(target, id),
                View::Opaque(_) => opaque_name(target),
            });
        }
        providers
    }

    /// Whether `file` could provide `name`: it exports it, it has lost it, it is
    /// opaque, or one of its stars could, with a cycle guard. A star whose path names
    /// no file could, as an opaque module could: what it held is exactly what is not
    /// known, and a moved import may have marked the barrel holding it whole.
    ///
    /// Nothing else links a barrel to an opaque module behind a star: `export *` is
    /// not a bare import.
    fn could_provide(&self, file: FileId, name: &str, seen: &mut AHashSet<FileId>) -> bool {
        if !seen.insert(file) {
            return false;
        }
        let fine = match self.view(file) {
            View::Fine(fine) => fine,
            // It could hold any name. See `opaque_name`.
            View::Opaque(_) => return true,
        };
        if fine.module().export_named(name).is_some() || self.has_lost(file, self.name_id(name)) {
            return true;
        }
        fine.module().export_stars.iter().any(|&source| {
            self.target_of(fine.analysed(), source)
                .is_none_or(|target| self.could_provide(target, name, seen))
        })
    }

    /// Every export of `file`, for a namespace import.
    fn all_exports(&self, file: FileId) -> Vec<Node> {
        // Not parsed to find it opaque, for the reason `lookup` gives.
        if !is_source_file(&self.path(file)) {
            return vec![opaque_name(file)];
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
        let fine = match self.view(file) {
            View::Fine(fine) => fine,
            View::Opaque(_) => {
                out.push(opaque_name(file));
                return;
            }
        };
        let (analysed, module) = (fine.analysed(), fine.module());

        for export in &module.exports {
            out.push(Node::Export(file, self.name_id(&export.name)));
        }
        // A namespace sees the whole table, including the shape of it, so a name that
        // has left is part of what it sees.
        for name in self.lost(file) {
            out.push(Node::Export(file, name));
        }
        for &source in &module.export_stars {
            if let Some(target) = self.target_of(analysed, source) {
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
            Node::Member(file, decl, member) => {
                let object = self
                    .analysis(file)
                    .and_then(|a| {
                        a.analysis
                            .as_fine()
                            .and_then(|m| m.decls.get(decl as usize).map(|d| d.name.clone()))
                    })
                    .unwrap_or_else(|| decl.to_string());
                format!("Member({}, {}.{})", path, object, self.name(member))
            }
            Node::ModuleInit(_) => format!("ModuleInit({})", path),
        }
    }
}

impl Default for Graph {
    fn default() -> Self {
        Self::new(
            Reading::default(),
            crate::config::Bundler::default(),
            std::sync::Arc::new(crate::resolve::Unresolved::default()),
            PathBuf::from("."),
            std::sync::Arc::new(crate::lockfile::Changed::default()),
            std::sync::Arc::default(),
            AHashMap::default(),
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

/// What a name looked up in an opaque file reaches, for every rule that looks one
/// up: the whole file. A leaf, an asset, a JSON module or a module the analyser
/// gave up on could hold any name, so it could be where the name comes from.
fn opaque_name(file: FileId) -> Node {
    Node::File(file)
}

/// The member `member` of the declaration `decl`, if its properties can be read
/// apart and it has that one.
fn member_of<'m>(module: &'m FineModule, decl: DeclId, member: &str) -> Option<&'m Member> {
    module
        .decls
        .get(decl as usize)?
        .members
        .iter()
        .find(|entry| entry.name == member)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::change::Change;
    use crate::config::{Bundler, Configs};

    /// A tree of the given files, and where it is.
    pub(crate) fn tree(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        for (name, body) in files {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        (dir, root)
    }

    pub(crate) fn reading(root: &Path) -> Reading {
        Reading {
            configs: Arc::new(Configs::new(root)),
            ignore_types: true,
        }
    }

    /// A graph over the tree at `root`, built with what `change` gives one.
    pub(crate) fn graph_of(root: &Path, change: &Change) -> Graph {
        let (packages, repointing) = change.for_resolution();
        Graph::new(
            reading(root),
            Bundler::default(),
            Arc::default(),
            root.to_path_buf(),
            packages,
            repointing,
            change.lost_exports(),
        )
    }

    /// A graph over a tree of the given files, and the directory holding them.
    pub(crate) fn graph_for(files: &[(&str, &str)]) -> (tempfile::TempDir, Graph) {
        let (dir, root) = tree(files);
        let change = Change::read(
            &root,
            crate::diff::ChangeSet::default(),
            &[],
            None,
            reading(&root),
        );
        let graph = graph_of(&root, &change);
        (dir, graph)
    }

    /// The file `name` of the tree `dir` holds.
    pub(crate) fn file(graph: &Graph, dir: &tempfile::TempDir, name: &str) -> FileId {
        graph.file_id(&dunce::canonicalize(dir.path()).unwrap().join(name))
    }

    /// Every node a search from `anchor` could arrive at, the anchor included.
    pub(crate) fn reach(graph: &Graph, anchor: FileId) -> AHashSet<Node> {
        let mut seen = AHashSet::default();
        let mut stack = vec![Node::File(anchor)];
        while let Some(node) = stack.pop() {
            if seen.insert(node) {
                stack.extend(graph.edges(node));
            }
        }
        seen
    }

    /// Nothing finer than the file can be said about an opaque one, so whatever
    /// names a part of it reaches the whole, and the whole reaches what it imports.
    /// A leaf and a module the analyser gave up on are the same to a reader.
    #[test]
    fn every_node_of_an_opaque_file_reaches_only_the_file() {
        let (dir, graph) = graph_for(&[
            (
                "legacy.js",
                "const a = require('./a');\nObject.assign(module.exports, { a });\n",
            ),
            ("a.ts", "export const a = 1;\n"),
            ("data.json", "{}\n"),
        ]);
        let legacy = file(&graph, &dir, "legacy.js");
        let a = file(&graph, &dir, "a.ts");
        let data = file(&graph, &dir, "data.json");
        assert!(graph.analysis(legacy).unwrap().analysis.as_fine().is_none());
        assert!(graph.analysis(data).is_none());

        assert_eq!(graph.edges(Node::File(legacy)), vec![Node::File(a)]);
        assert_eq!(graph.edges(Node::File(data)), Vec::<Node>::new());
        let name = graph.name_id("a");
        for opaque in [legacy, data] {
            for node in [
                Node::Decl(opaque, 0),
                Node::Export(opaque, name),
                Node::Member(opaque, 0, name),
                Node::ModuleInit(opaque),
            ] {
                assert_eq!(graph.edges(node), vec![Node::File(opaque)], "{node:?}");
            }
        }
        assert_eq!(reach(&graph, legacy).len(), 1 + reach(&graph, a).len());
    }

    /// A name the base revision exported and this one does not leaves no trace in
    /// the file, so the change says so and the graph is built knowing it. The node
    /// standing for the name is reached by the readers still asking for it, a
    /// namespace among them, and by nobody else.
    #[test]
    fn a_lost_export_is_reached_by_the_files_that_import_it() {
        let (_dir, root) = tree(&[
            ("lib.ts", "export const renamed = 1;\n"),
            (
                "user.ts",
                "import { original } from './lib';\nexport const u = original;\n",
            ),
            (
                "namespace.ts",
                "import * as lib from './lib';\nexport const n = lib;\n",
            ),
            (
                "other.ts",
                "import { renamed } from './lib';\nexport const o = renamed;\n",
            ),
        ]);
        let earlier: AHashMap<PathBuf, String> = [(
            root.join("lib.ts"),
            "export const original = 1;\n".to_string(),
        )]
        .into_iter()
        .collect();
        let change = Change::read(
            &root,
            crate::diff::ChangeSet::default(),
            &[PathBuf::from("lib.ts")],
            Some(Box::new(earlier)),
            reading(&root),
        );
        let graph = graph_of(&root, &change);
        let lost = Node::Export(
            graph.file_id(&root.join("lib.ts")),
            graph.name_id("original"),
        );
        let reaches = |name: &str| reach(&graph, graph.file_id(&root.join(name))).contains(&lost);

        assert!(reaches("user.ts"));
        assert!(reaches("namespace.ts"));
        assert!(!reaches("other.ts"));
    }

    /// Parsing a file resolves its imports, and a search records the ones that
    /// resolve nowhere for the files it arrives at, so naming a file that cannot be
    /// fine must not parse it.
    #[test]
    fn looking_a_name_up_in_a_stylesheet_does_not_parse_it() {
        let (dir, graph) = graph_for(&[("theme.css", "@import './missing.css';\n")]);
        let theme = file(&graph, &dir, "theme.css");

        assert_eq!(graph.resolve_export(theme, "x"), vec![Node::File(theme)]);
        assert_eq!(
            graph.resolve_member(theme, "x", "y"),
            vec![Node::File(theme)]
        );
        assert_eq!(graph.all_exports(theme), vec![Node::File(theme)]);
        assert!(!graph.analyses.borrow().contains_key(&theme));
    }

    /// However a name is read out of an opaque file — imported, re-exported, taken
    /// as a namespace, read as a member, passed along by `export *`, or called as a
    /// factory — what the reader reaches is the whole file.
    #[test]
    fn a_search_reaches_into_fine_files_only() {
        let (dir, graph) = graph_for(&[
            (
                "page.ts",
                "import { a, helper } from './barrel';\n\
                 import { thing, make } from './legacy';\n\
                 import * as legacy from './legacy';\n\
                 import * as data from './data.json';\n\
                 import styles from './page.css';\n\
                 export const made = make('x', () => 1);\n\
                 export const Page = () => [a, helper, thing.member, legacy.x, data, styles, made];\n",
            ),
            (
                "barrel.ts",
                "export * from './gone';\nexport * from './a';\nexport * from './legacy';\n\
                 export { helper as renamed } from './legacy';\n",
            ),
            ("a.ts", "import './page.css';\nexport const a = 1;\n"),
            (
                "legacy.js",
                "const a = require('./a');\nObject.assign(module.exports, { a });\n",
            ),
            ("data.json", "{}\n"),
            ("page.css", ".page { color: red; }\n"),
        ]);
        let page = file(&graph, &dir, "page.ts");
        let reached = reach(&graph, page);

        assert!(reached.contains(&Node::File(file(&graph, &dir, "legacy.js"))));
        assert!(reached.contains(&Node::File(file(&graph, &dir, "data.json"))));
        for node in reached {
            match node {
                Node::File(_) => {}
                // Importing a source module runs it, and the edge says so before
                // asking whether the module could be read. What it arrives at is
                // the whole file all the same.
                Node::ModuleInit(file) if graph.view(file).fine().is_none() => {
                    assert_eq!(graph.edges(node), vec![Node::File(file)]);
                }
                _ => assert!(
                    graph.view(node.file()).fine().is_some(),
                    "{}",
                    graph.render(node, Path::new("/"))
                ),
            }
        }
    }

    /// A graph over a project written to a temporary directory, which the caller
    /// keeps alive for as long as the graph reads from it.
    fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Graph) {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        for (path, content) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        (dir, root, Graph::default())
    }

    /// What the declaration `name` of `file` depends on.
    fn decl_edges(graph: &Graph, file: FileId, name: &str) -> Vec<Node> {
        let analysed = graph.analysis(file).unwrap();
        let module = analysed.analysis.as_fine().unwrap();
        let decl = module.decls.iter().position(|d| d.name == name).unwrap();
        graph.edges(Node::Decl(file, decl as DeclId))
    }

    #[test]
    fn a_star_from_a_file_with_no_analysis_stands_for_that_file() {
        let (_dir, root, graph) = project(&[
            ("src/tokens.json", "{ \"primary\": \"red\" }\n"),
            (
                "src/barrel.ts",
                "export * from \"./tokens.json\";\nexport const other = 1;\n",
            ),
            (
                "src/page.ts",
                "import { primary } from \"./barrel\";\nexport const Page = () => primary;\n",
            ),
        ]);
        let page = graph.file_id(&root.join("src/page.ts"));
        let tokens = graph.file_id(&root.join("src/tokens.json"));
        assert!(reaches(
            &graph,
            decl(&graph, page, "Page"),
            Node::File(tokens)
        ));
    }

    /// The node of the declaration `name` of `file`.
    fn decl(graph: &Graph, file: FileId, name: &str) -> Node {
        let analysed = graph.analysis(file).unwrap();
        let module = analysed.analysis.as_fine().unwrap();
        let decl = module.decls.iter().position(|d| d.name == name).unwrap();
        Node::Decl(file, decl as DeclId)
    }

    /// Whether any chain of edges leads from `from` to `to`, which is what a run
    /// asks, whatever nodes the chain passes through on the way.
    fn reaches(graph: &Graph, from: Node, to: Node) -> bool {
        let mut seen = AHashSet::default();
        let mut stack = vec![from];
        while let Some(node) = stack.pop() {
            if node == to {
                return true;
            }
            if seen.insert(node) {
                stack.extend(graph.edges(node));
            }
        }
        false
    }

    #[test]
    fn a_star_target_that_could_hold_any_name_does_not_hide_the_ones_after_it() {
        let (_dir, root, graph) = project(&[
            ("src/tokens.json", "{ \"primary\": \"red\" }\n"),
            ("src/star.ts", "export const x = 1;\nexport const y = 2;\n"),
            (
                "src/barrel.ts",
                "export * from \"./tokens.json\";\nexport * from \"./star\";\n",
            ),
            (
                "src/page.ts",
                "import { x } from \"./barrel\";\nexport const Page = () => x;\n",
            ),
        ]);
        let page = decl(&graph, graph.file_id(&root.join("src/page.ts")), "Page");
        let star = graph.file_id(&root.join("src/star.ts"));
        let tokens = graph.file_id(&root.join("src/tokens.json"));
        assert!(reaches(&graph, page, decl(&graph, star, "x")));
        assert!(reaches(&graph, page, Node::File(tokens)));
        assert!(!reaches(&graph, page, decl(&graph, star, "y")));
    }

    #[test]
    fn every_barrel_a_name_passes_through_stays_on_its_path() {
        let (_dir, root, graph) = project(&[
            ("src/tokens.json", "{ \"primary\": \"red\" }\n"),
            (
                "src/barrel.ts",
                "export * from \"./tokens.json\";\nglobalThis.ready = 1;\n",
            ),
            ("src/outer.ts", "export * from \"./barrel\";\n"),
            (
                "src/page.ts",
                "import { primary } from \"./outer\";\nexport const Page = () => primary;\n",
            ),
        ]);
        let page = graph.file_id(&root.join("src/page.ts"));
        let outer = graph.file_id(&root.join("src/outer.ts"));
        let barrel = graph.file_id(&root.join("src/barrel.ts"));
        let tokens = graph.file_id(&root.join("src/tokens.json"));
        let primary = graph.name_id("primary");
        assert!(decl_edges(&graph, page, "Page").contains(&Node::Export(outer, primary)));
        assert!(
            graph
                .edges(Node::Export(outer, primary))
                .contains(&Node::Export(barrel, primary))
        );
        assert!(
            graph
                .edges(Node::Export(barrel, primary))
                .contains(&Node::File(tokens))
        );
    }

    #[test]
    fn a_name_never_exported_reaches_every_file_behind_the_stars() {
        // `star` and `barrel` re-export each other, which the walk must survive.
        let (_dir, root, graph) = project(&[
            ("src/deep.ts", "export const d = 1;\n"),
            (
                "src/star.ts",
                "export * from \"./deep\";\nexport * from \"./barrel\";\nexport const y = 2;\n",
            ),
            (
                "src/barrel.ts",
                "export * from \"./star\";\nexport const z = 3;\n",
            ),
            (
                "src/page.ts",
                "import { x } from \"./barrel\";\nexport const Page = () => x;\n",
            ),
        ]);
        let page = graph.file_id(&root.join("src/page.ts"));
        let edges = decl_edges(&graph, page, "Page");
        for file in ["src/barrel.ts", "src/star.ts", "src/deep.ts"] {
            let file = graph.file_id(&root.join(file));
            assert!(edges.contains(&Node::File(file)), "{edges:?}");
        }
    }

    /// With inline requires, a module is evaluated where one of its names is first
    /// read, and a store's creator runs where the store is made. A creator that
    /// reads `b` in place of `a` makes loading the store's module evaluate `b`'s
    /// module and no longer `a`'s, so against a base revision a page that imports
    /// only the constant beside the store is reached. Without inline requires, both
    /// modules are evaluated as the store's module is, whatever the creator reads,
    /// and the page is not reached.
    #[test]
    fn a_creator_that_reads_another_import_changes_what_loading_evaluates_under_inline_requires() {
        let store = |read: &str| {
            format!(
                "import {{ create }} from \"zustand\";\nimport {{ a }} from \"./a\";\nimport {{ b }} from \"./b\";\n\
                 export const TITLE = \"Counter\";\n\
                 export const useCounter = create(() => ({{ value: {read} }}));\n"
            )
        };
        let (before, after) = (store("a"), store("b"));
        for (inline, expected) in [(true, true), (false, false)] {
            let mut files = vec![
                ("src/a.ts", "registerFeature(\"a\");\nexport const a = 1;\n"),
                ("src/b.ts", "export const b = 2;\n"),
                ("src/store.ts", after.as_str()),
                (
                    "src/page.tsx",
                    "import { TITLE } from \"./store\";\nexport const Page = () => <h1>{TITLE}</h1>;\n",
                ),
            ];
            if inline {
                files.push(("fallout.toml", "inline-requires = true\n"));
            }
            let (_dir, root) = tree(&files);
            let options = crate::Options {
                anchors: vec![PathBuf::from("src/page.tsx")],
                changed: vec![PathBuf::from("src/store.ts")],
                diff: None,
                base: Some("HEAD".to_string()),
                root: root.clone(),
                only: None,
                granularity: crate::Granularity::Symbol,
                include_types: false,
            };
            let earlier: AHashMap<PathBuf, String> =
                AHashMap::from_iter([(root.join("src/store.ts"), before.clone())]);
            let answer = crate::analyse_with(&options, Some(Box::new(earlier))).expect("an answer");
            assert_eq!(
                answer.verdict.is_affected(),
                expected,
                "inline requires: {inline}"
            );
        }
    }

    /// With inline requires, a declaration that reads an import as its module is
    /// evaluated evaluates the imported module there. One that stops reading it, or
    /// goes, no longer does, so against a base revision a page that imports only the
    /// constant beside it is reached: the top-level call in `tokens.ts` no longer
    /// runs as it loads. Without inline requires, importing `tokens.ts` evaluates it
    /// whatever reads it, and the page is not reached.
    #[test]
    fn a_declaration_that_stops_reading_an_import_changes_what_loading_evaluates_under_inline_requires()
     {
        let before = "import { base } from \"./tokens\";\nexport const label = \"sizes\";\nexport const small = base;\n";
        for after in [
            "import { base } from \"./tokens\";\nexport const label = \"sizes\";\nexport const small = 0;\n",
            "import { base } from \"./tokens\";\nexport const label = \"sizes\";\n",
        ] {
            for (inline, expected) in [(true, true), (false, false)] {
                let mut files = vec![
                    (
                        "src/tokens.ts",
                        "registerTokens();\nexport const base = 4;\n",
                    ),
                    ("src/sizes.ts", after),
                    (
                        "src/page.tsx",
                        "import { label } from \"./sizes\";\nexport const Page = () => <h1>{label}</h1>;\n",
                    ),
                ];
                if inline {
                    files.push(("fallout.toml", "inline-requires = true\n"));
                }
                let (_dir, root) = tree(&files);
                let options = crate::Options {
                    anchors: vec![PathBuf::from("src/page.tsx")],
                    changed: vec![PathBuf::from("src/sizes.ts")],
                    diff: None,
                    base: Some("HEAD".to_string()),
                    root: root.clone(),
                    only: None,
                    granularity: crate::Granularity::Symbol,
                    include_types: false,
                };
                let earlier: AHashMap<PathBuf, String> =
                    AHashMap::from_iter([(root.join("src/sizes.ts"), before.to_string())]);
                let answer =
                    crate::analyse_with(&options, Some(Box::new(earlier))).expect("an answer");
                assert_eq!(
                    answer.verdict.is_affected(),
                    expected,
                    "{after} (inline requires: {inline})"
                );
            }
        }
    }
}
