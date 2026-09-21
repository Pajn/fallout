//! Top-level statements to declarations, and the import bindings they can reference.

use oxc_ast::ast::*;
use oxc_span::GetSpan;

use super::parse::span_of;
use super::{ImportRef, ImportTarget, SourceId, Span};

/// A declaration before its reference edges are known.
#[derive(Debug, Clone)]
pub(crate) struct DeclDraft {
    pub name: String,
    /// Span of the whole top-level statement, so an edit anywhere in it marks every
    /// declaration that statement introduces.
    pub span: Span,
    /// Index of the top-level statement this came from.
    pub statement: usize,
    /// `const` bound to a primitive literal, and nothing else. Only such a binding is
    /// provably not a channel between two declarations that both reference it.
    pub immutable: bool,
}

/// A binding introduced by an import statement.
#[derive(Debug, Clone)]
pub(crate) struct ImportBinding {
    pub local: String,
    pub reference: ImportRef,
    pub span: Span,
}

/// Every top-level declaration, in source order.
///
/// Returns `None` when a statement introduces bindings we cannot enumerate, which
/// coarsens the whole module.
pub(crate) fn collect(program: &Program<'_>) -> Option<Vec<DeclDraft>> {
    let mut drafts = Vec::new();

    for (index, statement) in program.body.iter().enumerate() {
        let span = span_of(statement.span());
        match statement {
            // `export const x = 1` / `export function x() {}`
            Statement::ExportDeclaration(export) => {
                push_declaration(&export.declaration, span, index, &mut drafts)?;
            }
            Statement::ExportDefaultDeclaration(_) => {
                // The default export is a declaration in its own right, named for the
                // slot it occupies rather than for any binding.
                drafts.push(DeclDraft {
                    name: "default".to_string(),
                    span,
                    statement: index,
                    immutable: false,
                });
            }
            statement => {
                if let Some(declaration) = statement.as_declaration() {
                    push_declaration(declaration, span, index, &mut drafts)?;
                }
            }
        }
    }

    Some(drafts)
}

fn push_declaration(
    declaration: &Declaration<'_>,
    span: Span,
    statement: usize,
    drafts: &mut Vec<DeclDraft>,
) -> Option<()> {
    match declaration {
        Declaration::VariableDeclaration(variable) => {
            for declarator in &variable.declarations {
                // Destructuring binds several names; each is its own declaration.
                let names = declarator.id.get_binding_identifiers();
                if names.is_empty() {
                    return None;
                }
                // Only a plain `const x = <literal>` is provably immutable. A
                // destructured binding is not, however it is initialised.
                let immutable = variable.kind == VariableDeclarationKind::Const
                    && names.len() == 1
                    && declarator.init.as_ref().is_some_and(is_primitive_literal);
                for name in names {
                    drafts.push(DeclDraft {
                        name: name.name.to_string(),
                        span,
                        statement,
                        immutable,
                    });
                }
            }
        }
        Declaration::FunctionDeclaration(function) => {
            let id = function.id.as_ref()?;
            drafts.push(DeclDraft {
                name: id.name.to_string(),
                span,
                statement,
                immutable: false,
            });
        }
        Declaration::ClassDeclaration(class) => {
            let id = class.id.as_ref()?;
            drafts.push(DeclDraft {
                name: id.name.to_string(),
                span,
                statement,
                immutable: false,
            });
        }
        // A type cannot hold a runtime value, so it is never a mutation channel.
        Declaration::TSTypeAliasDeclaration(alias) => drafts.push(DeclDraft {
            name: alias.id.name.to_string(),
            span,
            statement,
            immutable: true,
        }),
        Declaration::TSInterfaceDeclaration(interface) => drafts.push(DeclDraft {
            name: interface.id.name.to_string(),
            span,
            statement,
            immutable: true,
        }),
        Declaration::TSEnumDeclaration(enumeration) => drafts.push(DeclDraft {
            name: enumeration.id.name.to_string(),
            span,
            statement,
            immutable: false,
        }),
        // A namespace or an import-equals merges across statements; the Coarsener
        // has already rejected the file, but be explicit rather than silent.
        _ => return None,
    }
    Some(())
}

/// Every binding an import statement introduces, with what it points at.
pub(crate) fn collect_imports(
    program: &Program<'_>,
    sources: &[String],
) -> Option<Vec<ImportBinding>> {
    let mut bindings = Vec::new();

    for statement in &program.body {
        let Statement::ImportDeclaration(import) = statement else {
            continue;
        };
        let source = source_id(sources, import.source.value.as_str())?;
        let span = span_of(import.span());

        let Some(specifiers) = &import.specifiers else {
            // A bare `import "./x"` introduces no binding; it is an init edge.
            continue;
        };

        for specifier in specifiers {
            let (local, target) = match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(named) => (
                    named.local.name.to_string(),
                    ImportTarget::Named(named.imported.name().to_string()),
                ),
                ImportDeclarationSpecifier::ImportDefaultSpecifier(default) => (
                    default.local.name.to_string(),
                    ImportTarget::Named("default".to_string()),
                ),
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(namespace) => {
                    (namespace.local.name.to_string(), ImportTarget::Namespace)
                }
            };

            bindings.push(ImportBinding {
                local,
                reference: ImportRef { source, target },
                span,
            });
        }
    }

    Some(bindings)
}

/// Bare imports: `import "./theme.css"`. Importing the module runs their effects, so
/// they belong to module initialisation rather than to any declaration.
pub(crate) fn bare_sources(program: &Program<'_>, sources: &[String]) -> Vec<SourceId> {
    let mut bare = Vec::new();
    for statement in &program.body {
        let Statement::ImportDeclaration(import) = statement else {
            continue;
        };
        let is_bare = import
            .specifiers
            .as_ref()
            .is_none_or(|specifiers| specifiers.is_empty());
        if !is_bare {
            continue;
        }
        if let Some(source) = source_id(sources, import.source.value.as_str()) {
            if !bare.contains(&source) {
                bare.push(source);
            }
        }
    }
    bare
}

/// A literal with no interior: nothing a later statement could reach into and change.
fn is_primitive_literal(expression: &Expression<'_>) -> bool {
    matches!(
        expression,
        Expression::StringLiteral(_)
            | Expression::NumericLiteral(_)
            | Expression::BooleanLiteral(_)
            | Expression::NullLiteral(_)
            | Expression::BigIntLiteral(_)
    )
}

pub(crate) fn source_id(sources: &[String], specifier: &str) -> Option<SourceId> {
    sources
        .iter()
        .position(|s| s == specifier)
        .map(|i| i as SourceId)
}
