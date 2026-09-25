//! The export table.

use oxc_ast::ast::*;
use oxc_span::GetSpan;

use super::cjs;
use super::decls::{DeclDraft, ImportBinding, source_id};
use super::parse::span_of;
use super::{DeclId, Export, ExportTarget, ImportTarget, SourceId};

/// Builds the export table, plus the `export * from` sources to be resolved lazily.
///
/// Returns `None` when an export cannot be described, which coarsens the module.
pub(crate) fn collect(
    program: &Program<'_>,
    sources: &[String],
    drafts: &[DeclDraft],
    imports: &[ImportBinding],
    cjs: &cjs::Table,
) -> Option<(Vec<Export>, Vec<SourceId>)> {
    let mut exports: Vec<Export> = Vec::new();
    let mut stars: Vec<SourceId> = Vec::new();

    for (index, statement) in program.body.iter().enumerate() {
        let span = span_of(statement.span());

        // `exports.x = …`, which named a declaration of its own in `decls`.
        for name in cjs.names_at(index) {
            let local = cjs::decl_name(name);
            let decl = drafts
                .iter()
                .position(|draft| draft.statement == index && draft.name == local)?;
            exports.push(Export {
                name: name.to_string(),
                target: ExportTarget::Local(decl as DeclId),
                span,
            });
        }

        match statement {
            // `export const x = 1` / `export function x() {}`
            Statement::ExportDeclaration(export) => {
                let span = span_of(export.span());
                for decl in decls_from_statement(drafts, index) {
                    exports.push(Export {
                        name: drafts[decl as usize].name.clone(),
                        target: ExportTarget::Local(decl),
                        span,
                    });
                }
            }
            // `export { a, b as c }`, naming a local declaration or a binding an
            // import introduced.
            Statement::ExportNamedDeclaration(export) => {
                let span = span_of(export.span());
                for specifier in &export.specifiers {
                    let local = specifier.local.name();
                    let target = match drafts
                        .iter()
                        .position(|d| d.name.as_str() == local.as_str())
                    {
                        Some(decl) => ExportTarget::Local(decl as DeclId),
                        // Nothing here declares the name, so an import brought it in
                        // and this is a re-export written in two statements. It says
                        // exactly what `export { a } from "./g"` says, and is read
                        // the same way rather than giving up on the whole module —
                        // which is the shape every barrel of namespaces has.
                        None => reexported(imports, local.as_str())?,
                    };
                    exports.push(Export {
                        name: specifier.exported.name().as_str().to_string(),
                        target,
                        span,
                    });
                }
            }
            // `export { a as b } from "./g"`
            Statement::ExportFromDeclaration(export) => {
                let span = span_of(export.span());
                let source = source_id(sources, export.source.value.as_str())?;
                for specifier in &export.specifiers {
                    exports.push(Export {
                        name: specifier.exported.name().as_str().to_string(),
                        target: ExportTarget::Reexport {
                            source,
                            name: specifier.local.name().as_str().to_string(),
                        },
                        span,
                    });
                }
            }
            Statement::ExportDefaultDeclaration(export) => {
                let span = span_of(export.span());
                let decl = decls_from_statement(drafts, index).first().copied()?;
                exports.push(Export {
                    name: "default".to_string(),
                    target: ExportTarget::Local(decl),
                    span,
                });
            }
            Statement::ExportAllDeclaration(export) => {
                let source = source_id(sources, export.source.value.as_str())?;
                match &export.exported {
                    // `export * as ns from "./g"` exports the single name `ns`,
                    // which holds everything `g` exports. Read as a star it would
                    // put `g`'s names in this table instead of its own, and `ns`
                    // would be a name nothing here has — which is how a consumer
                    // asking for it ended up reaching nothing at all.
                    Some(exported) => exports.push(Export {
                        name: exported.name().as_str().to_string(),
                        target: ExportTarget::ReexportAll { source },
                        span: span_of(export.span()),
                    }),
                    // `export * from "./g"` has no name of its own: `g`'s table is
                    // copied into this one, resolved lazily.
                    None => {
                        if !stars.contains(&source) {
                            stars.push(source);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Some((exports, stars))
}

/// The export target for a name an import introduced, or `None` if no import did —
/// which coarsens, since nothing here can say what the name stands for.
fn reexported(imports: &[ImportBinding], local: &str) -> Option<ExportTarget> {
    let binding = imports.iter().find(|binding| binding.local == local)?;
    let source = binding.reference.source;
    Some(match &binding.reference.target {
        // An import statement names whole exports; a member is only ever read later.
        ImportTarget::Named(name) | ImportTarget::Member { export: name, .. } => {
            ExportTarget::Reexport {
                source,
                name: name.clone(),
            }
        }
        ImportTarget::Namespace => ExportTarget::ReexportAll { source },
    })
}

fn decls_from_statement(drafts: &[DeclDraft], index: usize) -> Vec<DeclId> {
    drafts
        .iter()
        .enumerate()
        .filter(|(_, draft)| draft.statement == index)
        .map(|(id, _)| id as DeclId)
        .collect()
}
