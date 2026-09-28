//! What runs when the module is evaluated.
//!
//! `ModuleInit(f)` is the node every importer of `f` depends on. It collects the
//! declarations whose values are computed at import time, either because a top-level
//! statement uses them or because their own initialiser may have side effects.

use oxc_ast::ast::*;
use oxc_span::GetSpan;

use ahash::AHashSet;

use super::decls::DeclDraft;
use super::parse::{Ctx, span_of};
use super::side_effects::SideEffects;
use super::{Decl, DeclId};

/// Declarations module initialisation depends on.
pub(super) fn collect(
    ctx: &Ctx<'_>,
    program: &Program<'_>,
    drafts: &[DeclDraft],
    decls: &[Decl],
    effects: &SideEffects<'_, '_>,
    conditional: &AHashSet<usize>,
) -> (Vec<DeclId>, Vec<DeclId>) {
    let mut init: Vec<DeclId> = Vec::new();
    let mut maybe: Vec<DeclId> = Vec::new();

    for (index, statement) in program.body.iter().enumerate() {
        let declares = drafts.iter().any(|draft| draft.statement == index);

        if !declares {
            // A top-level statement that declares nothing runs for its effect. Every
            // declaration it names is part of initialisation.
            for decl in referenced_decls(ctx, statement, drafts) {
                push_unique(&mut init, decl);
            }
            continue;
        }

        // An initialiser that may have side effects runs at import time whether or
        // not anyone reads the binding. One [`SideEffects`] clears is not asked about
        // again below.
        if !effects.declaring_runs(statement) {
            continue;
        }

        // A call that may be a factory's runs nothing else if its arguments do not:
        // whether the call itself does is the graph's to say, once it knows the
        // callee. See [`super::factories`].
        if conditional.contains(&index)
            && let Some((_, call)) = super::factories::call_of(statement)
            && !call.arguments.iter().any(|argument| {
                argument
                    .as_expression()
                    .is_none_or(|argument| effects.runs(argument))
            })
        {
            for (id, draft) in drafts.iter().enumerate() {
                if draft.statement == index {
                    push_unique(&mut maybe, id as DeclId);
                }
            }
            continue;
        }

        for (id, draft) in drafts.iter().enumerate() {
            if draft.statement == index {
                push_unique(&mut init, id as DeclId);
            }
        }
    }

    // Initialisation reaches whatever those declarations reach, including an object
    // one of them reads a property of.
    let mut queue: Vec<DeclId> = init.clone();
    while let Some(current) = queue.pop() {
        let entry = &decls[current as usize];
        let objects = entry.member_refs.iter().map(|(object, _)| object);
        for &next in entry.refs.iter().chain(objects) {
            if !init.contains(&next) {
                init.push(next);
                queue.push(next);
            }
        }
    }

    // A conditional declaration an effectful one reads is part of initialisation
    // either way, so it may be in both lists.
    init.sort_unstable();
    (init, maybe)
}

/// Top-level declarations named anywhere inside `statement`.
fn referenced_decls(ctx: &Ctx<'_>, statement: &Statement<'_>, drafts: &[DeclDraft]) -> Vec<DeclId> {
    let scoping = ctx.semantic.scoping();
    let root = scoping.root_scope_id();
    let span = span_of(statement.span());

    let mut found = Vec::new();
    for symbol_id in scoping.symbol_ids() {
        if scoping.symbol_scope_id(symbol_id) != root {
            continue;
        }
        let name = scoping.symbol_name(symbol_id);
        let Some(decl) = drafts.iter().position(|d| d.name == name) else {
            continue;
        };

        for reference_id in scoping.get_resolved_reference_ids(symbol_id) {
            let node_id = scoping.get_reference(*reference_id).node_id();
            let at = span_of(ctx.semantic.nodes().get_node(node_id).kind().span());
            if span.contains(at.start) {
                push_unique(&mut found, decl as DeclId);
                break;
            }
        }
    }
    found
}

fn push_unique(list: &mut Vec<DeclId>, value: DeclId) {
    if !list.contains(&value) {
        list.push(value);
    }
}

#[cfg(test)]
mod tests {
    use crate::module::{FineModule, ModuleAnalysis, Reading, parse::analyse_source};
    use std::path::Path;

    fn module(source: &str) -> Box<FineModule> {
        let (ModuleAnalysis::Fine(module), _) =
            analyse_source(Path::new("lib.ts"), source, &Reading::default()).unwrap()
        else {
            panic!("expected fine module: {source}")
        };
        module
    }

    #[test]
    fn a_shadowed_pure_import_in_a_factory_argument_makes_the_call_initialisation() {
        let shadowed = module(
            "import { createAsyncThunk } from '@reduxjs/toolkit';
            import { memo } from 'react';
            export const t = createAsyncThunk('a/b', class {
                static { const memo = () => register(1); memo(); }
            });\n",
        );
        let t = shadowed.decl_named("t").unwrap();
        assert!(shadowed.init_decls.contains(&t));
        assert!(!shadowed.conditional_init.contains(&t));

        let imported = module(
            "import { createAsyncThunk } from '@reduxjs/toolkit';
            import { memo } from 'react';
            export const t = createAsyncThunk('a/b', class { static { memo(1); } });\n",
        );
        let t = imported.decl_named("t").unwrap();
        assert!(!imported.init_decls.contains(&t));
        assert!(imported.conditional_init.contains(&t));
    }
}
