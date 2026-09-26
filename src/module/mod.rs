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
mod globals;
pub mod init;
mod local_pure;
mod members;
pub mod parse;
pub mod refs;
mod shared;
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
    /// `const f = X.withTypes<T>()`, where `X` is an import or a declaration here.
    pub derived: Option<Callee>,
    /// The call this declaration's value is the result of, `const t = f(...)`,
    /// with what each argument depends on. The graph decides whether `f` is a
    /// factory it knows; see [`crate::factories`].
    pub factory: Option<FactoryCall>,
}

/// A value named by an import or a declaration of this file, read through a path of
/// properties: `rtk.createAsyncThunk` is `Import { name: "*", path: ["createAsyncThunk"] }`
/// for `import * as rtk`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callee {
    Import {
        source: SourceId,
        /// The name the target exports, `default` or `*`.
        name: String,
        path: Vec<String>,
    },
    Local {
        decl: DeclId,
        path: Vec<String>,
    },
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
    /// Each argument's span, and what it depends on.
    pub args: Vec<(Span, Deps)>,
    /// What the declaration depends on outside every argument: the callee, a type
    /// annotation, and every edge of the shared-state rule.
    pub frame: Deps,
    /// Inside the parentheses. An edit here that touches no argument touches only
    /// the commas and the space between them.
    pub interior: Span,
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
