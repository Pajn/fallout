//! The export table.

use oxc_ast::ast::*;
use oxc_span::GetSpan;

use super::cjs;
use super::decls::{DeclDraft, source_id};
use super::parse::span_of;
use super::{DeclId, Export, ExportTarget, SourceId};

/// Builds the export table, plus the `export * from` sources to be resolved lazily.
///
/// Returns `None` when an export cannot be described, which coarsens the module.
pub(crate) fn collect(
    program: &Program<'_>,
    sources: &[String],
    drafts: &[DeclDraft],
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
            // `export { a, b as c }` naming existing local declarations.
            Statement::ExportNamedDeclaration(export) => {
                let span = span_of(export.span());
                for specifier in &export.specifiers {
                    let local = specifier.local.name();
                    let decl = drafts
                        .iter()
                        .position(|d| d.name.as_str() == local.as_str())?;
                    exports.push(Export {
                        name: specifier.exported.name().as_str().to_string(),
                        target: ExportTarget::Local(decl as DeclId),
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
            // `export * from "./g"`, and `export * as ns from "./g"`, which is
            // coarser than it needs to be but never finer.
            Statement::ExportAllDeclaration(export) => {
                let source = source_id(sources, export.source.value.as_str())?;
                if !stars.contains(&source) {
                    stars.push(source);
                }
            }
            _ => {}
        }
    }

    Some((exports, stars))
}

fn decls_from_statement(drafts: &[DeclDraft], index: usize) -> Vec<DeclId> {
    drafts
        .iter()
        .enumerate()
        .filter(|(_, draft)| draft.statement == index)
        .map(|(id, _)| id as DeclId)
        .collect()
}
