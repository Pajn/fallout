//! Reading a single module.
//!
//! Every analysis has a "give up" answer, and giving up always means
//! [`ModuleAnalysis::Coarse`]: one opaque node for the whole file, which is exactly
//! the file-level behaviour. Nothing here may ever conclude "not affected".

pub mod cjs;
pub mod compare;
pub mod decls;
pub mod exports;
pub mod init;
pub mod parse;
pub mod refs;
pub mod types;

use std::path::Path;

pub use parse::{LineTable, SOURCE_EXTENSIONS, is_source_file};

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
    /// Imported bindings this one references.
    pub imports: Vec<ImportRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    /// Callees the project has declared free of side effects.
    pub pure: crate::pure::PureList,
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
            pure: crate::pure::PureList::default(),
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
