//! Reading a single module.
//!
//! Every analysis has a "give up" answer, and giving up always means
//! [`ModuleAnalysis::Coarse`]: one opaque node for the whole file, which is exactly
//! the file-level behaviour. Nothing here may ever conclude "not affected".

pub mod cjs;
pub mod compare;
pub mod decls;
pub mod exports;
mod factories;
pub mod init;
mod members;
pub mod parse;
pub mod refs;
mod shared;
mod side_effects;
pub mod style;
pub mod types;

use std::path::Path;

pub use parse::{LineTable, SOURCE_EXTENSIONS, is_source_file};
pub use style::{STYLE_EXTENSIONS, is_style_file};

/// Index into [`FineModule::decls`].
pub type DeclId = u32;
/// Index into [`FineModule::sources`].
pub type SourceId = u32;

/// One top-level declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decl {
    pub name: String,
    /// Span of the whole top-level statement that declares it, for hunk attribution.
    pub span: Span,
    /// Declarations in this same file that this one references.
    pub refs: Vec<DeclId>,
    /// Properties of object declarations in this same file that this one reads,
    /// where it reads them one at a time: `utils.formatDate` is `(utils, formatDate)`.
    pub member_refs: Vec<(DeclId, String)>,
    /// Imported bindings this one references.
    pub imports: Vec<ImportRef>,
    /// The properties of the plain object literal this declaration binds, where
    /// each can be read on its own. Empty for anything else, and for an object
    /// whose members could be reached or changed some other way.
    pub members: Vec<Member>,
    /// The inside of that object literal, between its braces. An edit here that
    /// touches no member touches only the separators between them.
    pub interior: Span,
    /// What this declaration's value is another name for: `const f = X` and
    /// `const f = X.withTypes<T>()`, where `X` is an import or a declaration here
    /// and `withTypes` is a method some rule declares an identity form, and
    /// `const f = X<T>()`, where `X` is an import.
    pub derived: Option<Callee>,
    /// The call this declaration's value is the result of, `const t = f(...)`,
    /// with what each argument depends on. The graph decides whether `f` is a
    /// factory it knows; see [`crate::factories`].
    pub factory: Option<FactoryCall>,
}

/// A value named by an import or a declaration of this file, read through a path of
/// steps: `base.withTypes<T>()` is `Import { name: "createAsyncThunk", path:
/// [Prop("withTypes"), Call] }` for `import { createAsyncThunk as base }`. A property
/// read straight off a namespace import is the name it imports, so `rtk.createAsyncThunk`
/// is `Import { name: "createAsyncThunk", path: [] }` for `import * as rtk`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callee {
    Import {
        source: SourceId,
        /// The name the target exports, `default` or `*`.
        name: String,
        path: Vec<Step>,
    },
    Local {
        decl: DeclId,
        path: Vec<Step>,
    },
}

impl Callee {
    fn path_mut(&mut self) -> &mut Vec<Step> {
        match self {
            Callee::Import { path, .. } | Callee::Local { path, .. } => path,
        }
    }
}

/// One step of reading a [`Callee`] further.
///
/// A call is kept as a step of its own rather than read through, because what a
/// call returns is the callee's to say: `withTypes<T>()` returns RTK's factory
/// itself, and the same call on anything else returns anything. Which calls hand
/// back what they were called on is declared by each rule; see
/// [`crate::factories::rules::Identity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `.name`
    Prop(String),
    /// `()`, with no arguments.
    Call,
}

/// What part of a declaration depends on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deps {
    pub refs: Vec<DeclId>,
    pub member_refs: Vec<(DeclId, String)>,
    pub imports: Vec<ImportRef>,
}

/// `const t = f(a, b)`: the callee, and what each argument depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactoryCall {
    pub callee: Callee,
    pub args: Vec<Argument>,
    /// What the declaration depends on outside every argument: the callee, a type
    /// annotation, every edge of the shared-state rule, and anything no reference
    /// accounts for, such as an import or `require` inside an argument.
    pub frame: Deps,
    /// Inside the parentheses. An edit here that touches no argument touches only
    /// the commas and the space between them.
    pub interior: Span,
    /// Where an argument the call does not pass would be written: after the last
    /// one and its trailing comma, if it has one, up to the closing parenthesis.
    /// Removing that argument leaves its mark here.
    pub missing: Span,
}

/// One argument of a [`FactoryCall`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Argument {
    pub span: Span,
    /// What it depends on.
    pub deps: Deps,
    /// Whether calling it once, where the call is written, with arguments nobody
    /// here knows anything about, is proven to run nothing and not to throw: a
    /// function written out in place, whose body the local-helper proof clears.
    ///
    /// Evaluating a function runs nothing, whatever its body does, so this is the
    /// question for a factory that calls what it is given while creating its value.
    pub quiet_when_called: bool,
    /// Whether calling it may read an imported binding, directly or through a
    /// declaration of this file it names, such as a helper it calls. Where a
    /// project defers each import to its first use, reading one evaluates the
    /// module it names, which is not quiet. Which reads would happen when is not
    /// told apart, so one inside a function the argument only creates counts too.
    pub reads_imports: bool,
}

/// One property of an object literal declaration, and what reading it depends on.
///
/// Every declaration of the statement shares the same members, since
/// `exports.a = exports.b = { ... }` names one object twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    /// Span of the whole property, key and value.
    pub span: Span,
    /// Declarations in this file the property's value references.
    pub refs: Vec<DeclId>,
    /// Properties of object declarations in this file the value reads, including
    /// a sibling that writes a binding this one reads.
    pub member_refs: Vec<(DeclId, String)>,
    /// Imported bindings the property's value references.
    pub imports: Vec<ImportRef>,
    /// Whether calling the property through the object, `utils.fn()`, may read the
    /// object as `this`, which reaches every other property. True for any value not
    /// known to be something else: an imported function, a local one that reads
    /// `this`, a call's result.
    pub receiver: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn contains(&self, offset: u32) -> bool {
        offset >= self.start && offset < self.end
    }

    pub fn intersects(&self, start: u32, end: u32) -> bool {
        // A zero-width position counts as touching the statement it falls inside.
        if start == end {
            return self.contains(start);
        }
        start < self.end && end > self.start
    }

    /// Whether the part of the range `start..end` that falls inside `outer` lies
    /// wholly inside this span: an edit to a statement that stays between the braces
    /// of its literal, or the parentheses of its call.
    pub fn holds_within(&self, outer: Span, start: u32, end: u32) -> bool {
        if start == end {
            return self.contains(start);
        }
        start.max(outer.start) >= self.start && end.min(outer.end) <= self.end
    }
}

/// What an imported binding points at inside the target module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportTarget {
    /// One named export. `default` for a default import.
    Named(String),
    /// One property of a named export: `utils.formatDate` read off `import { utils }`,
    /// or off `ns.utils`. The target reaches only that property when the export is a
    /// plain object literal, and the whole export otherwise.
    Member { export: String, member: String },
    /// Every export: `import * as ns`, and dynamic `import()` / `require()`.
    ///
    /// A binding import of a non-source file resolves through `Named`, which lands on
    /// `File(target)` because an asset has no export table.
    Namespace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRef {
    pub source: SourceId,
    pub target: ImportTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportTarget {
    /// `export const x`, `export { a as b }`
    Local(DeclId),
    /// `export { a as b } from "./g"` — the name is `a`, as spelled in `g`.
    Reexport { source: SourceId, name: String },
    /// `export * as ns from "./g"`: one name, holding everything `g` exports.
    ///
    /// Not the same as `export * from "./g"`, which has no name of its own and
    /// copies `g`'s table into this one. This exports a single binding, and reaching
    /// it reaches all of `g`'s exports.
    ReexportAll { source: SourceId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub name: String,
    pub target: ExportTarget,
    /// Span of the export statement, for hunk attribution.
    pub span: Span,
}

/// A module the analyser could describe declaration by declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FineModule {
    pub decls: Vec<Decl>,
    pub exports: Vec<Export>,
    /// `export * from "./g"`: g's export table is copied in, resolved lazily.
    pub export_stars: Vec<SourceId>,
    /// Every import specifier in the file, deduplicated, in source order.
    pub sources: Vec<String>,
    /// Declarations whose value is computed when the module is evaluated, either
    /// because a top-level statement references them or because their own
    /// initialiser may have side effects.
    pub init_decls: Vec<DeclId>,
    /// Bare `import "./x"` specifiers: importing this module runs their effects.
    pub bare_sources: Vec<SourceId>,
    /// Spans of the import statements, paired with the declarations that reference
    /// the bindings they introduce. Editing an import line marks those declarations.
    pub import_spans: Vec<(Span, Vec<DeclId>)>,
    /// Declarations whose initialiser runs nothing but a call to a callee that may
    /// be a factory the graph knows, or `withTypes` of one. Module initialisation
    /// reaches each one the graph cannot prove made by such a factory.
    pub conditional_init: Vec<DeclId>,
    /// Declarations whose initialiser, as it runs at load, reads an imported
    /// binding: itself, or in a local function it calls there. Where a project
    /// defers each import to its first use, that evaluates the module the binding
    /// names as this one is evaluated, so the graph counts them as initialisation
    /// then. Of a call that may be a factory's, only its arguments are asked about,
    /// since the call around them is reached whenever the call is.
    pub reads_on_load: Vec<DeclId>,
    /// Imported bindings read by the top-level statements that declare nothing,
    /// which run for their effect and are initialisation already. Where each
    /// import is deferred to its first use, reading one there is what evaluates
    /// its module.
    pub init_imports: Vec<ImportRef>,
}

impl FineModule {
    pub fn decl_named(&self, name: &str) -> Option<DeclId> {
        self.decls
            .iter()
            .position(|d| d.name == name)
            .map(|i| i as DeclId)
    }

    pub fn export_named(&self, name: &str) -> Option<&Export> {
        self.exports.iter().find(|e| e.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleAnalysis {
    /// One opaque node. Its import list is still usable: extracting specifiers works
    /// even when nothing else does, which is what the file-level tool relies on.
    Coarse {
        sources: Vec<String>,
    },
    Fine(Box<FineModule>),
}

impl ModuleAnalysis {
    /// Every import specifier, whether or not the module could be analysed finely.
    pub fn sources(&self) -> &[String] {
        match self {
            ModuleAnalysis::Coarse { sources } => sources,
            ModuleAnalysis::Fine(module) => &module.sources,
        }
    }

    pub fn as_fine(&self) -> Option<&FineModule> {
        match self {
            ModuleAnalysis::Fine(module) => Some(module),
            ModuleAnalysis::Coarse { .. } => None,
        }
    }
}

/// What a run reads a module as.
///
/// Both of these are decided once, by the command line and the project's config, and
/// then travel together wherever a module is read — including to the earlier version
/// of a file, which has to be read the same way as the current one or the two cannot
/// be compared.
#[derive(Clone)]
pub struct Reading {
    /// What each file's own directory chain declares, including the callees the
    /// project calls free of side effects. See [`crate::config`].
    pub configs: std::sync::Arc<crate::config::Configs>,
    /// Erase type-only syntax before reading anything, so that a change made only of
    /// types reaches nobody. See [`types`].
    ///
    /// On unless a run asks otherwise. The question this tool answers is whether a
    /// change can alter what a user sees, and a type cannot: it can fail the build,
    /// which fails every page at once and needs no answer about reachability.
    pub ignore_types: bool,
}

impl Default for Reading {
    fn default() -> Self {
        Self {
            configs: std::sync::Arc::new(crate::config::Configs::new(Path::new("."))),
            ignore_types: true,
        }
    }
}

/// Analyses one file, falling back to [`ModuleAnalysis::Coarse`] whenever anything is
/// not understood.
pub fn analyse(path: &Path, reading: &Reading) -> Option<(ModuleAnalysis, LineTable)> {
    parse::analyse_file(path, reading)
}

/// Every import specifier written in `path`, in source order.
///
/// `None` means the file has no outgoing edges to offer: it is a leaf, or it could
/// not be read.
///
/// Which callees a project calls pure cannot change this list, but how the file is
/// read can: an `import type` is not an import at all, and with `--ignore-types` it
/// is gone before anything counts the specifiers.
pub fn imported_specifiers(path: &Path, reading: &Reading) -> Option<Vec<String>> {
    Some(analyse(path, reading)?.0.sources().to_vec())
}
