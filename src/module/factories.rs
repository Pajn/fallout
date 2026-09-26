//! Calls that may be made by a factory the graph knows.
//!
//! `export const refresh = createAsyncThunk("session/refresh", async () => …)` makes
//! an object whose `fulfilled` depends on the first argument and nothing else. Which
//! function `createAsyncThunk` is cannot be told from here: an app usually calls
//! its own `createAsyncThunk.withTypes<…>()`, exported from a module of its own. So
//! this records what the graph needs to decide it:
//!
//! - which declarations are another name for an import or a declaration here,
//!   `const f = X` and `const f = X.withTypes<T>()`;
//! - which are the result of calling one, `const t = f(…)`, with what each argument
//!   depends on kept apart;
//! - which of those run nothing but that call, so that module initialisation need
//!   not reach them if the callee turns out to be a factory that only builds values.
//!
//! A call's result is read by property only while nothing can change what a
//! property holds: its binding is used only to read a property, to call it, or to
//! export it.

use ahash::{AHashMap, AHashSet};
use oxc_ast::AstKind;
use oxc_ast::ast::*;
use oxc_semantic::SymbolId;
use oxc_span::GetSpan;

use super::decls::{DeclDraft, ImportBinding};
use super::members::ObjectRef;
use super::parse::{Ctx, span_of};
use super::refs::{SharedEdges, narrowed, unwritten_member_read};
use super::{Callee, Decl, DeclId, Deps, FactoryCall, ImportTarget, Span};

/// What [`find`] found, before any reference is linked.
#[derive(Default)]
pub(crate) struct Candidates {
    /// The binding of each call whose result may be read by property.
    pub by_symbol: Vec<(SymbolId, DeclId)>,
    /// Statements whose initialiser runs nothing but one call, whether that call is
    /// a factory's or `withTypes` of one.
    pub conditional: AHashSet<usize>,
    calls: Vec<Pending>,
    derived: Vec<(DeclId, Callee)>,
}

struct Pending {
    decl: DeclId,
    callee: Callee,
    args: Vec<Span>,
    statement: Span,
    /// Inside the parentheses: from the end of the callee, and of any type
    /// arguments, to the closing parenthesis.
    interior: Span,
}

/// The call a `const` binds, `const t = f(…)`, exported or not.
pub(crate) fn call_of<'s, 'a>(
    statement: &'s Statement<'a>,
) -> Option<(&'s BindingIdentifier<'a>, &'s CallExpression<'a>)> {
    let (id, init) = const_binding(statement)?;
    match init.get_inner_expression() {
        Expression::CallExpression(call) => Some((id, call)),
        _ => None,
    }
}

fn const_binding<'s, 'a>(
    statement: &'s Statement<'a>,
) -> Option<(&'s BindingIdentifier<'a>, &'s Expression<'a>)> {
    let variable = match statement {
        Statement::VariableDeclaration(variable) => variable,
        Statement::ExportDeclaration(export) => match &export.declaration {
            Declaration::VariableDeclaration(variable) => variable,
            _ => return None,
        },
        _ => return None,
    };
    if variable.kind != VariableDeclarationKind::Const || variable.declarations.len() != 1 {
        return None;
    }
    let declarator = &variable.declarations[0];
    let BindingPattern::BindingIdentifier(id) = &declarator.id else {
        return None;
    };
    Some((id, declarator.init.as_ref()?))
}

pub(crate) fn find<'a>(
    ctx: &Ctx<'_>,
    program: &'a Program<'a>,
    drafts: &[DeclDraft],
    imports: &[ImportBinding],
) -> Candidates {
    let mut candidates = Candidates::default();
    let names = Names::new(drafts, imports);
    let decl_of = |index: usize, id: &BindingIdentifier<'_>| {
        names.by_statement.get(&(index, id.name.as_str())).copied()
    };
    let mut derived: AHashSet<DeclId> = AHashSet::default();

    // Other names first, since a call names its factory by one.
    for (index, statement) in program.body.iter().enumerate() {
        let Some((id, init)) = const_binding(statement) else {
            continue;
        };
        let (Some(decl), Some(callee)) = (decl_of(index, id), callee_of(ctx, init, &names)) else {
            continue;
        };
        // `X.withTypes<T>()` is a call, and runs nothing if `X` is a factory.
        if matches!(init.get_inner_expression(), Expression::CallExpression(_)) {
            candidates.conditional.insert(index);
        }
        derived.insert(decl);
        candidates.derived.push((decl, callee));
    }

    for (index, statement) in program.body.iter().enumerate() {
        let Some((id, init)) = const_binding(statement) else {
            continue;
        };
        let Some(decl) = decl_of(index, id) else {
            continue;
        };
        if derived.contains(&decl) {
            continue;
        }
        let Expression::CallExpression(call) = init.get_inner_expression() else {
            continue;
        };
        // A factory is imported, or is another name for one here. A function this
        // file declares is not one, and calling it is plain initialisation.
        let callee = match callee_of(ctx, &call.callee, &names) {
            Some(Callee::Local { decl: local, path }) if derived.contains(&local) => {
                Callee::Local { decl: local, path }
            }
            Some(callee @ Callee::Import { .. }) => callee,
            _ => continue,
        };
        let mut args = Vec::with_capacity(call.arguments.len());
        for argument in &call.arguments {
            // A spread is arguments nobody here can count.
            if argument.as_expression().is_none() {
                break;
            }
            args.push(span_of(argument.span()));
        }
        if args.len() != call.arguments.len() {
            continue;
        }
        candidates.conditional.insert(index);
        if let Some(symbol) = id.symbol_id.get()
            && read_by_property(ctx, symbol, span_of(statement.span()))
        {
            candidates.by_symbol.push((symbol, decl));
        }
        let opens = call
            .type_arguments
            .as_ref()
            .map_or(call.callee.span().end, |types| types.span.end);
        candidates.calls.push(Pending {
            decl,
            callee,
            args,
            statement: span_of(statement.span()),
            interior: Span {
                start: opens,
                end: call.span.end.saturating_sub(1),
            },
        });
    }
    candidates
}

/// The import or declaration an expression names, read through properties and
/// through `withTypes<T>()`, which RTK gives its factories to type them and which
/// returns the factory itself.
/// The file's top-level names, looked up once per statement.
struct Names<'d> {
    decls: AHashMap<&'d str, DeclId>,
    imports: AHashMap<&'d str, &'d ImportBinding>,
    by_statement: AHashMap<(usize, &'d str), DeclId>,
}

impl<'d> Names<'d> {
    fn new(drafts: &'d [DeclDraft], imports: &'d [ImportBinding]) -> Self {
        let mut decls = AHashMap::default();
        let mut by_statement = AHashMap::default();
        for (id, draft) in drafts.iter().enumerate() {
            decls.entry(draft.name.as_str()).or_insert(id as DeclId);
            by_statement.insert((draft.statement, draft.name.as_str()), id as DeclId);
        }
        Self {
            decls,
            imports: imports
                .iter()
                .map(|binding| (binding.local.as_str(), binding))
                .collect(),
            by_statement,
        }
    }
}

fn callee_of(ctx: &Ctx<'_>, expr: &Expression<'_>, names: &Names<'_>) -> Option<Callee> {
    match expr.get_inner_expression() {
        Expression::Identifier(identifier) => {
            let scoping = ctx.semantic.scoping();
            let symbol = scoping
                .get_reference(identifier.reference_id.get()?)
                .symbol_id()?;
            if scoping.symbol_scope_id(symbol) != scoping.root_scope_id() {
                return None;
            }
            let name = identifier.name.as_str();
            if let Some(binding) = names.imports.get(name) {
                let name = match &binding.reference.target {
                    ImportTarget::Named(name) => name.clone(),
                    ImportTarget::Namespace => "*".to_string(),
                    ImportTarget::Member { .. } => return None,
                };
                return Some(Callee::Import {
                    source: binding.reference.source,
                    name,
                    path: Vec::new(),
                });
            }
            Some(Callee::Local {
                decl: *names.decls.get(name)?,
                path: Vec::new(),
            })
        }
        Expression::StaticMemberExpression(member) => {
            let mut callee = callee_of(ctx, &member.object, names)?;
            let property = member.property.name.to_string();
            match &mut callee {
                Callee::Import { name, path, .. } if name == "*" && path.is_empty() => {
                    *name = property;
                }
                Callee::Import { path, .. } | Callee::Local { path, .. } => path.push(property),
            }
            Some(callee)
        }
        Expression::CallExpression(call) if call.arguments.is_empty() => {
            let Expression::StaticMemberExpression(member) = call.callee.get_inner_expression()
            else {
                return None;
            };
            (member.property.name == "withTypes")
                .then(|| callee_of(ctx, &member.object, names))
                .flatten()
        }
        _ => None,
    }
}

/// Every use of the binding reads one property and writes nothing through it,
/// calls it, or exports it. Any other use hands it to code that could change what
/// one of its properties holds.
fn read_by_property(ctx: &Ctx<'_>, symbol: SymbolId, statement: Span) -> bool {
    let scoping = ctx.semantic.scoping();
    let nodes = ctx.semantic.nodes();
    scoping.get_resolved_reference_ids(symbol).iter().all(|id| {
        let reference = scoping.get_reference(*id);
        let node_id = reference.node_id();
        let span = nodes.get_node(node_id).kind().span();
        if reference.is_write() || statement.contains(span.start) {
            return false;
        }
        match nodes.parent_kind(node_id) {
            AstKind::ExportSpecifier(_) | AstKind::ExportDefaultDeclaration(_) => true,
            AstKind::CallExpression(call) if call.callee.span() == span => true,
            _ => unwritten_member_read(nodes, node_id).is_some(),
        }
    })
}

/// Fills in each call's arguments, once every edge of its declaration is known.
///
/// A reference is attributed to the argument it is written in. What the
/// declaration depends on outside every argument, and every edge the shared-state
/// rule gave it, goes to the frame, which every member reached through the call
/// depends on.
pub(crate) fn attach(
    ctx: &Ctx<'_>,
    drafts: &[DeclDraft],
    imports: &[ImportBinding],
    objects: &AHashMap<SymbolId, ObjectRef>,
    candidates: Candidates,
    shared: &SharedEdges,
    decls: &mut [Decl],
) {
    for (decl, callee) in candidates.derived {
        decls[decl as usize].derived = Some(callee);
    }
    if candidates.calls.is_empty() {
        return;
    }

    let scoping = ctx.semantic.scoping();
    let nodes = ctx.semantic.nodes();
    let root = scoping.root_scope_id();
    let decl_by_name: AHashMap<&str, DeclId> = drafts
        .iter()
        .enumerate()
        .map(|(id, draft)| (draft.name.as_str(), id as DeclId))
        .collect();
    let import_by_name: AHashMap<&str, &ImportBinding> = imports
        .iter()
        .map(|binding| (binding.local.as_str(), binding))
        .collect();

    let mut args: Vec<Vec<Deps>> = candidates
        .calls
        .iter()
        .map(|call| vec![Deps::default(); call.args.len()])
        .collect();
    for symbol_id in scoping.symbol_ids() {
        if scoping.symbol_scope_id(symbol_id) != root {
            continue;
        }
        let name = scoping.symbol_name(symbol_id);
        let target_decl = decl_by_name.get(name).copied();
        let target_import = import_by_name.get(name).copied();
        if target_decl.is_none() && target_import.is_none() {
            continue;
        }
        let object = objects.get(&symbol_id);
        for reference_id in scoping.get_resolved_reference_ids(symbol_id) {
            let node_id = scoping.get_reference(*reference_id).node_id();
            let at = nodes.get_node(node_id).kind().span().start;
            for (index, call) in candidates.calls.iter().enumerate() {
                if !call.statement.contains(at) {
                    continue;
                }
                let Some(argument) = call.args.iter().position(|span| span.contains(at)) else {
                    continue;
                };
                let deps = &mut args[index][argument];
                if let Some(target) = target_decl
                    && target != call.decl
                {
                    match object.zip(unwritten_member_read(nodes, node_id)) {
                        Some((object, read)) => {
                            push_unique(&mut deps.member_refs, (object.decl, read))
                        }
                        None => push_unique(&mut deps.refs, target),
                    }
                }
                if let Some(binding) = target_import {
                    push_unique(
                        &mut deps.imports,
                        narrowed(nodes, node_id, &binding.reference),
                    );
                }
            }
        }
    }

    for (call, args) in candidates.calls.into_iter().zip(args) {
        let entry = &decls[call.decl as usize];
        let shared = shared.get(&call.decl);
        let mut frame = Deps {
            refs: entry
                .refs
                .iter()
                .copied()
                .filter(|target| {
                    shared.is_some_and(|shared| shared.contains(target))
                        || !args.iter().any(|deps| deps.refs.contains(target))
                })
                .collect(),
            member_refs: entry
                .member_refs
                .iter()
                .filter(|read| !args.iter().any(|deps| deps.member_refs.contains(read)))
                .cloned()
                .collect(),
            imports: entry
                .imports
                .iter()
                .filter(|import| !args.iter().any(|deps| deps.imports.contains(import)))
                .cloned()
                .collect(),
        };
        frame.refs.retain(|&target| target != call.decl);
        decls[call.decl as usize].factory = Some(FactoryCall {
            callee: call.callee,
            args: call.args.into_iter().zip(args).collect(),
            frame,
            interior: call.interior,
        });
    }
}

fn push_unique<T: PartialEq>(list: &mut Vec<T>, value: T) {
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
            analyse_source(Path::new("slice.ts"), source, &Reading::default()).unwrap()
        else {
            panic!("expected fine module: {source}")
        };
        module
    }

    const HEAD: &str = "import { createAsyncThunk } from '@reduxjs/toolkit';
        function load() { return 1; }
        const TYPE = 'a/b';
        export const t = createAsyncThunk(TYPE, async () => load());\n";

    #[test]
    fn each_argument_keeps_what_it_depends_on() {
        let module = module(HEAD);
        let t = module.decl_named("t").unwrap() as usize;
        let name = |id: &u32| module.decls[*id as usize].name.clone();
        let call = module.decls[t].factory.as_ref().expect("a call");
        let refs: Vec<Vec<String>> = call
            .args
            .iter()
            .map(|(_, deps)| deps.refs.iter().map(name).collect())
            .collect();
        assert_eq!(refs, [vec!["TYPE".to_string()], vec!["load".to_string()]]);
        // The callee is an import, which is the frame's.
        assert!(call.frame.refs.is_empty());
        assert_eq!(call.frame.imports.len(), 1);
        // Creating the thunk runs nothing but the call, so it waits on the graph.
        assert!(module.conditional_init.contains(&(t as u32)));
        assert!(!module.init_decls.contains(&(t as u32)));
    }

    #[test]
    fn a_thunk_is_read_by_property_where_it_is_only_read_called_or_exported() {
        let reader = "export const r = () => t.fulfilled;\n";
        for (rest, narrowed) in [
            ("export const call = () => t();\nexport default t;\n", true),
            ("export const pass = () => wrap(t);\n", false),
            (
                "export const write = () => { t.fulfilled = other; };\n",
                false,
            ),
        ] {
            let module = module(&format!("{HEAD}{reader}{rest}"));
            let t = module.decl_named("t").unwrap();
            let r = module.decl_named("r").unwrap() as usize;
            let read = (t, "fulfilled".to_string());
            assert_eq!(
                module.decls[r].member_refs.contains(&read),
                narrowed,
                "{rest}"
            );
            assert_eq!(module.decls[r].refs.contains(&t), !narrowed, "{rest}");
        }
    }

    #[test]
    fn a_call_to_a_function_of_this_file_is_plain_initialisation() {
        let module = module(
            "function make(type: string) { return register(type); }
            export const t = make('a/b');\n",
        );
        let t = module.decl_named("t").unwrap();
        assert!(module.init_decls.contains(&t));
        assert!(module.conditional_init.is_empty());
    }

    #[test]
    fn a_call_whose_arguments_run_something_is_initialisation_whatever_the_callee() {
        let module = module(
            "import { createAsyncThunk } from '@reduxjs/toolkit';
            export const t = createAsyncThunk(register('a/b'), async () => 1);\n",
        );
        let t = module.decl_named("t").unwrap();
        assert!(module.init_decls.contains(&t));
        assert!(!module.conditional_init.contains(&t));
    }
}
