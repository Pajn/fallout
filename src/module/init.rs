//! What runs when the module is evaluated.
//!
//! `ModuleInit(f)` is the node every importer of `f` depends on. It collects the
//! declarations whose values are computed at import time, either because a top-level
//! statement uses them or because their own initialiser may have side effects.
//!
//! What code run at load reads of the imports is collected too. A statement that
//! declares nothing depends on the values it reads, and has no node to reach them
//! through. And reading an import runs nothing of this file's, but where a project
//! defers each import to its first use it evaluates the module the import names,
//! and the graph, which knows whether the project does, counts a declaration that
//! reads one then.

use oxc_ast::ast::*;
use oxc_span::GetSpan;

use ahash::AHashSet;

use super::decls::DeclDraft;
use super::parse::{Ctx, span_of};
use super::refs::StatementImports;
use super::side_effects::SideEffects;
use super::{Decl, DeclId, ImportRef};

/// What module initialisation depends on. See [`super::FineModule`] for each part.
pub(super) struct Initialisation {
    pub decls: Vec<DeclId>,
    pub conditional: Vec<DeclId>,
    pub reads_on_load: Vec<DeclId>,
    pub imports: Vec<ImportRef>,
}

/// What module initialisation depends on.
pub(super) fn collect(
    ctx: &Ctx<'_>,
    program: &Program<'_>,
    drafts: &[DeclDraft],
    decls: &[Decl],
    effects: &SideEffects<'_, '_>,
    conditional: &AHashSet<usize>,
    statement_imports: &StatementImports,
) -> Initialisation {
    let mut init: Vec<DeclId> = Vec::new();
    let mut maybe: Vec<DeclId> = Vec::new();
    let mut reads_on_load: Vec<DeclId> = Vec::new();
    let mut imports: Vec<ImportRef> = Vec::new();

    for (index, statement) in program.body.iter().enumerate() {
        let declares = drafts.iter().any(|draft| draft.statement == index);
        let declared = || {
            drafts
                .iter()
                .enumerate()
                .filter(move |(_, draft)| draft.statement == index)
                .map(|(id, _)| id as DeclId)
        };

        if !declares {
            // A top-level statement that declares nothing runs for its effect. Every
            // declaration it names is part of initialisation, and so is every import
            // it reads, except where it only forwards a name: `export { base }`
            // reads nothing until a name is read through this module.
            for decl in referenced_decls(ctx, statement, drafts) {
                push_unique(&mut init, decl);
            }
            if !matches!(statement, Statement::ExportNamedDeclaration(_)) {
                for import in statement_imports.get(&index).into_iter().flatten() {
                    if !imports.contains(import) {
                        imports.push(import.clone());
                    }
                }
            }
            continue;
        }

        // Of a call that may be a factory's, only the arguments are asked about. The
        // callee and the call around them are reached whenever the call is, and what
        // an argument the factory calls reads is the graph's to weigh with the
        // factory.
        let call = conditional
            .contains(&index)
            .then(|| super::factories::call_of(statement))
            .flatten();
        let reads = match call {
            Some((_, call)) => call.arguments.iter().any(|argument| {
                argument
                    .as_expression()
                    .is_none_or(|argument| effects.reads_import(argument))
            }),
            None => effects.declaring_reads_import(statement),
        };
        if reads {
            for id in declared() {
                push_unique(&mut reads_on_load, id);
            }
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
    Initialisation {
        decls: init,
        conditional: maybe,
        reads_on_load,
        imports,
    }
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

    /// The declarations whose initialiser reads an import as it runs at load, by name.
    fn reading(source: &str, types_ignored: bool) -> Vec<String> {
        let reading = Reading {
            ignore_types: types_ignored,
            ..Reading::default()
        };
        let (ModuleAnalysis::Fine(module), _) =
            analyse_source(Path::new("lib.tsx"), source, &reading).unwrap()
        else {
            panic!("expected fine module: {source}")
        };
        module
            .reads_on_load
            .iter()
            .map(|&id| module.decls[id as usize].name.clone())
            .collect()
    }

    #[test]
    fn a_declaration_that_reads_an_import_as_it_runs_at_load_is_recorded() {
        for (source, name) in [
            (
                "import { base } from './tokens';\nexport const x = base;\n",
                "x",
            ),
            // Reading a member off the module object reads the import too.
            (
                "import * as tokens from './tokens';\nexport const x = tokens.base;\n",
                "x",
            ),
            // A proven helper called where the declaration is written runs its body
            // there, whether it is a function declaration or a `const` arrow.
            (
                "import { base } from './tokens';\nfunction scaled(n) { return { base, n }; }\nexport const x = scaled(2);\n",
                "x",
            ),
            (
                "import { base } from './tokens';\nconst scaled = (n) => ({ base, n });\nexport const x = scaled(2);\n",
                "x",
            ),
            // And so does one that another helper calls.
            (
                "import { base } from './tokens';\nfunction inner() { return base; }\nfunction outer() { return inner(); }\nexport const x = outer();\n",
                "x",
            ),
            (
                "import { base } from './tokens';\nexport class Widget { static size = base; }\n",
                "Widget",
            ),
            (
                "import { base } from './tokens';\nexport const { size = base } = {};\n",
                "size",
            ),
            (
                "import { base } from './tokens';\nexport default base;\n",
                "default",
            ),
            (
                "import { Badge } from './badge';\nexport const badge = <Badge />;\n",
                "badge",
            ),
            // Of a call that may be a factory's, an argument read as the call is
            // made counts, as it does for any call.
            (
                "import { createWithEqualityFn } from 'zustand/traditional';\nimport { same } from './equality';\nexport const useStore = createWithEqualityFn(() => ({ count: 0 }), same);\n",
                "useStore",
            ),
        ] {
            assert!(
                reading(source, true).contains(&name.to_string()),
                "{source}"
            );
        }
    }

    #[test]
    fn a_read_through_a_local_const_is_recorded_on_the_const() {
        // Every top-level declaration runs at load, so the const that reads the import
        // is recorded itself, and what reads the const is not: it evaluates nothing
        // the const has not.
        let source =
            "import { base } from './tokens';\nconst local = base;\nexport const x = local;\n";
        assert_eq!(reading(source, true), ["local"]);
    }

    #[test]
    fn a_read_that_waits_for_a_call_or_is_a_type_is_not_recorded() {
        for source in [
            "import { base } from './tokens';\nexport const read = () => base;\n",
            "import { base } from './tokens';\nexport function read() { return base; }\n",
            // A helper called only inside a function that nothing calls at load.
            "import { base } from './tokens';\nfunction scaled(n) { return { base, n }; }\nexport const later = (n) => scaled(n);\n",
            // An instance field waits for a construction, and a method for a call.
            "import { base } from './tokens';\nexport class Widget { size = base; read() { return base; } }\n",
            // Reading a helper without calling it runs nothing of it.
            "import { base } from './tokens';\nfunction scaled() { return base; }\nexport const alias = scaled;\n",
            // Making a store calls its creator, which the graph judges with the
            // factory. The callee is reached with the call around the arguments.
            "import { create } from 'zustand';\nimport { base } from './tokens';\nexport const useStore = create(() => ({ base }));\n",
        ] {
            assert!(reading(source, true).is_empty(), "{source}");
        }
        // A type-only import loads nothing, read as written or with its types erased.
        for source in [
            "import type { Size } from './tokens';\nexport const x: Size = 1;\n",
            "import { type Size } from './tokens';\nexport const x = 1 as Size;\n",
            "import { Size } from './tokens';\nexport const x: Size = 1;\n",
        ] {
            for types_ignored in [true, false] {
                assert!(
                    reading(source, types_ignored).is_empty(),
                    "{source} (types ignored: {types_ignored})"
                );
            }
        }
    }

    #[test]
    fn an_import_a_statement_that_declares_nothing_reads_is_kept_for_initialisation() {
        let read = module("import { base } from './tokens';\nconsole.info(base);\n");
        assert_eq!(read.init_imports.len(), 1);
        // Forwarding a name reads nothing: the module it names is evaluated when a
        // name is read through this one.
        let forwarded = module("import { base } from './tokens';\nexport { base };\n");
        assert!(forwarded.init_imports.is_empty());
    }
}
