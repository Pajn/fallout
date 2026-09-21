//! Reference edges between declarations, and the shared-state rule.

use ahash::AHashMap;
use oxc_semantic::SymbolId;
use oxc_span::GetSpan;

use super::decls::{DeclDraft, ImportBinding};
use super::parse::{Ctx, span_of};
use super::{Decl, DeclId, ImportRef, Span};

/// Fills in each declaration's references, and returns the import statement spans
/// paired with the declarations that use the bindings those statements introduce.
pub(crate) fn link(
    ctx: &Ctx<'_>,
    drafts: &[DeclDraft],
    imports: &[ImportBinding],
    decls: &mut [Decl],
) -> Vec<(Span, Vec<DeclId>)> {
    let scoping = ctx.semantic.scoping();
    let root = scoping.root_scope_id();

    // Which declarations does each top-level statement introduce?
    let mut statement_decls: AHashMap<usize, Vec<DeclId>> = AHashMap::default();
    for (id, draft) in drafts.iter().enumerate() {
        statement_decls
            .entry(draft.statement)
            .or_default()
            .push(id as DeclId);
    }

    let decl_by_name: AHashMap<&str, DeclId> = drafts
        .iter()
        .enumerate()
        .map(|(id, draft)| (draft.name.as_str(), id as DeclId))
        .collect();
    let import_by_name: AHashMap<&str, &ImportBinding> = imports
        .iter()
        .map(|binding| (binding.local.as_str(), binding))
        .collect();

    // Declarations referencing each module-scope binding, for the shared-state rule.
    let mut users_of: AHashMap<SymbolId, Vec<DeclId>> = AHashMap::default();
    // Declarations referencing each import statement's bindings, for hunk attribution.
    let mut import_users: AHashMap<Span, Vec<DeclId>> = AHashMap::default();

    for symbol_id in scoping.symbol_ids() {
        if scoping.symbol_scope_id(symbol_id) != root {
            continue;
        }
        let name = scoping.symbol_name(symbol_id);
        let target_decl = decl_by_name.get(name).copied();
        let target_import = import_by_name.get(name).copied();

        for reference_id in scoping.get_resolved_reference_ids(symbol_id) {
            let node_id = scoping.get_reference(*reference_id).node_id();
            let span = span_of(ctx.semantic.nodes().get_node(node_id).kind().span());

            // Which top-level statement is this reference written in?
            let Some(statement) = ctx.statement_at(span.start) else {
                continue;
            };
            // A reference from a statement that declares nothing is module
            // initialisation, which `init` handles.
            let Some(users) = statement_decls.get(&statement) else {
                continue;
            };

            for &user in users {
                if let Some(target) = target_decl {
                    // A declaration referring to itself is not an edge.
                    if user != target {
                        push_unique(&mut decls[user as usize].refs, target);
                    }
                }
                if let Some(binding) = target_import {
                    push_import(&mut decls[user as usize].imports, binding.reference.clone());
                    push_unique(import_users.entry(binding.span).or_default(), user);
                }
                push_unique(users_of.entry(symbol_id).or_default(), user);
            }
        }
    }

    apply_shared_state(drafts, &decl_by_name, &users_of, ctx, decls);

    let mut spans: Vec<(Span, Vec<DeclId>)> = import_users.into_iter().collect();
    spans.sort_by_key(|(span, _)| *span);
    spans
}

/// If two declarations both reference a module-scope binding that is not provably
/// immutable, a change to one can reach the other by mutating it. They collapse into
/// one strongly connected group.
fn apply_shared_state(
    drafts: &[DeclDraft],
    decl_by_name: &AHashMap<&str, DeclId>,
    users_of: &AHashMap<SymbolId, Vec<DeclId>>,
    ctx: &Ctx<'_>,
    decls: &mut [Decl],
) {
    let scoping = ctx.semantic.scoping();

    for (symbol_id, users) in users_of {
        if users.len() < 2 {
            continue;
        }

        // Only a binding declared in this file holds a module-scope value that two
        // declarations here could pass between them. An imported binding is the
        // exporting module's business, and is already an edge to that module.
        let name = scoping.symbol_name(*symbol_id);
        let Some(&declared) = decl_by_name.get(name) else {
            continue;
        };
        if drafts[declared as usize].immutable {
            continue;
        }

        for &user in users {
            for &other in users {
                if user != other {
                    push_unique(&mut decls[user as usize].refs, other);
                }
            }
        }
    }
}

fn push_unique(list: &mut Vec<DeclId>, value: DeclId) {
    if !list.contains(&value) {
        list.push(value);
    }
}

fn push_import(list: &mut Vec<ImportRef>, value: ImportRef) {
    if !list.contains(&value) {
        list.push(value);
    }
}
