//! Calls that may be made by a factory the graph knows.
//!
//! `export const refresh = createAsyncThunk("session/refresh", async () => …)` makes
//! an object whose `fulfilled` depends on the first argument and nothing else. Which
//! function `createAsyncThunk` is cannot be told from here: an app usually calls
//! its own `createAsyncThunk.withTypes<…>()`, exported from a module of its own. So
//! this records what the graph needs to decide it:
//!
//! - which declarations are another name for an import or a declaration here,
//!   `const f = X`, `const f = X.withTypes<T>()` and, for an import `X`,
//!   `const f = X<T>()`;
//! - which are the result of calling one, `const t = f(…)`, with what each argument
//!   depends on kept apart, and whether calling it there would run anything, for a
//!   factory that calls what it is given;
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
use super::side_effects::{SideEffects, Wrapped};
use super::{Argument, Callee, Decl, DeclId, Deps, FactoryCall, ImportTarget, Span, Step};
use crate::factories::rules::{is_identity_call, is_identity_method};

/// What [`find`] found, before any reference is linked.
#[derive(Default)]
pub(crate) struct Candidates {
    /// The binding of each call whose result may be read by property.
    pub by_symbol: Vec<(SymbolId, DeclId)>,
    /// Statements whose initialiser runs nothing but one call, whether that call is
    /// a factory's or an identity form of one, such as `withTypes`.
    pub conditional: AHashSet<usize>,
    calls: Vec<Pending>,
    derived: Vec<(DeclId, Callee)>,
    /// Every `require` and `import()` a declaration makes, by where it is written.
    /// Filled in once they are found, which is after the calls are.
    pub placed: super::refs::Placed,
}

struct Pending {
    decl: DeclId,
    callee: Callee,
    args: Vec<Span>,
    /// Whether calling each argument where the call is written runs nothing, with
    /// the middleware that proof looks through. See
    /// [`super::Argument::quiet_when_called`].
    quiet: Vec<Option<Vec<Wrapped>>>,
    /// The top-level statement the call is written in.
    statement: usize,
    /// Inside the parentheses: from the end of the callee, and of any type
    /// arguments, to the closing parenthesis.
    interior: Span,
    missing: Span,
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
    effects: &SideEffects<'_, '_>,
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
        // `X.withTypes<T>()` and `X<T>()` are calls, and run nothing if `X` is a
        // factory whose rule declares them.
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
        let mut quiet = Vec::with_capacity(call.arguments.len());
        for argument in &call.arguments {
            // A spread is arguments nobody here can count.
            let Some(expression) = argument.as_expression() else {
                break;
            };
            args.push(span_of(argument.span()));
            quiet.push(effects.called_quietly(expression, call.span.start));
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
        let closes = call.span.end.saturating_sub(1);
        let missing = missing_argument(ctx, args.last().map_or(opens, |span| span.end), closes);
        candidates.calls.push(Pending {
            decl,
            callee,
            args,
            quiet,
            statement: index,
            interior: Span {
                start: opens,
                end: closes,
            },
            missing,
        });
    }
    candidates
}

/// Where an argument after the last would be written, between the end of the last
/// argument, `after`, and the closing parenthesis at `closes`.
///
/// Without a trailing comma, taking a later argument away edits the line the last
/// one ends on, so that line is the place. With one, that line stays as it was and
/// the removal lands on a later one, so the place starts after it. A line of the
/// last argument that is edited with its comma left alone is then no edit to an
/// argument the call does not pass.
fn missing_argument(ctx: &Ctx<'_>, after: u32, closes: u32) -> Span {
    let text = ctx
        .semantic
        .source_text()
        .get(after as usize..closes as usize)
        .unwrap_or("");
    let start = match text.find(',') {
        Some(comma) => {
            let rest = &text[comma + 1..];
            let line = rest
                .find('\n')
                .map_or(comma + 1, |newline| comma + 1 + newline + 1);
            after + line as u32
        }
        None => after,
    };
    // The closing parenthesis is included, so that a removal on its line counts.
    Span {
        start: start.min(closes),
        end: closes + 1,
    }
}

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

/// The import or declaration an expression names, read through properties and
/// through calls such as `withTypes<T>()`, which RTK gives its factories to type
/// them and which returns the factory itself.
///
/// Whether such a call returns what it was called on is the matched rule's to say,
/// so it is kept as a step for the graph. Only a call with no arguments is read at
/// all, and only where some rule declares such a call an identity form: to a method
/// of that name, or, for Zustand's `create<T>()`, to an import itself. Any other call's
/// result is a value no rule speaks for, and calling it is plain initialisation.
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
                callee => callee.path_mut().push(Step::Prop(property)),
            }
            Some(callee)
        }
        Expression::CallExpression(call) if call.arguments.is_empty() => {
            let method = match call.callee.get_inner_expression() {
                Expression::StaticMemberExpression(member) => {
                    is_identity_method(&member.property.name)
                }
                _ => false,
            };
            let mut callee = callee_of(ctx, &call.callee, names)?;
            // Called with no arguments, it is the import itself that may be the
            // factory. A function this file declares is plain initialisation to
            // call, as it is with arguments, and so is anything read off an import.
            let imported = matches!(&callee, Callee::Import { path, .. } if path.is_empty());
            if !method && !(is_identity_call() && imported) {
                return None;
            }
            callee.path_mut().push(Step::Call);
            Some(callee)
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
/// A reference is attributed by where it is written: to the argument it is in, or,
/// outside every argument, to the frame, which every member reached through the
/// call depends on. A name read in both places is in both. So is a `require` or an
/// `import()`, which the candidates' `placed` says where to find. What the declaration depends on
/// that none of these accounts for, and every edge the shared-state rule gave it,
/// goes to the frame too.
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
    let call_by_statement: AHashMap<usize, usize> = candidates
        .calls
        .iter()
        .enumerate()
        .map(|(index, call)| (call.statement, index))
        .collect();

    let mut args: Vec<Vec<Deps>> = candidates
        .calls
        .iter()
        .map(|call| vec![Deps::default(); call.args.len()])
        .collect();
    let mut frames: Vec<Deps> = vec![Deps::default(); candidates.calls.len()];
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
            let Some(&index) = ctx
                .statement_at(at)
                .and_then(|statement| call_by_statement.get(&statement))
            else {
                continue;
            };
            let call = &candidates.calls[index];
            for deps in attributed(call, &mut args[index], &mut frames[index], at) {
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

    for (at, import) in &candidates.placed {
        let Some(&index) = ctx
            .statement_at(*at)
            .and_then(|statement| call_by_statement.get(&statement))
        else {
            continue;
        };
        let call = &candidates.calls[index];
        for deps in attributed(call, &mut args[index], &mut frames[index], *at) {
            push_unique(&mut deps.imports, import.clone());
        }
    }

    for ((call, args), mut frame) in candidates.calls.into_iter().zip(args).zip(frames) {
        let entry = &decls[call.decl as usize];
        let attributed = || args.iter().chain(std::iter::once(&frame));
        let refs: Vec<DeclId> = entry
            .refs
            .iter()
            .copied()
            .filter(|target| !attributed().any(|deps| deps.refs.contains(target)))
            .chain(shared.get(&call.decl).into_iter().flatten().copied())
            .collect();
        let member_refs: Vec<_> = entry
            .member_refs
            .iter()
            .filter(|read| !attributed().any(|deps| deps.member_refs.contains(read)))
            .cloned()
            .collect();
        let imports: Vec<_> = entry
            .imports
            .iter()
            .filter(|import| !attributed().any(|deps| deps.imports.contains(import)))
            .cloned()
            .collect();
        for target in refs {
            push_unique(&mut frame.refs, target);
        }
        for read in member_refs {
            push_unique(&mut frame.member_refs, read);
        }
        for import in imports {
            push_unique(&mut frame.imports, import);
        }
        frame.refs.retain(|&target| target != call.decl);
        let args = call
            .args
            .into_iter()
            .zip(args)
            .zip(call.quiet)
            .map(|((span, deps), quiet)| Argument {
                span,
                reads_imports: reads_imports(decls, &deps),
                deps,
                quiet_when_called: quiet.is_some(),
                wrappers: quiet
                    .into_iter()
                    .flatten()
                    .map(|wrapped| wrapped.source)
                    .collect(),
            })
            .collect();
        decls[call.decl as usize].factory = Some(FactoryCall {
            callee: call.callee,
            args,
            frame,
            interior: call.interior,
            missing: call.missing,
        });
    }
}

/// What a reference written at `at`, in the statement of `call`, is attributed to:
/// the argument it is in, or the frame outside every argument. A middleware's callee
/// is in an argument, but it runs as the store is made, as the factory does, so a
/// reference in one is the frame's too.
fn attributed<'d>(
    call: &Pending,
    args: &'d mut [Deps],
    frame: &'d mut Deps,
    at: u32,
) -> Vec<&'d mut Deps> {
    let Some(argument) = call.args.iter().position(|span| span.contains(at)) else {
        return vec![frame];
    };
    let wrapper = call.quiet[argument]
        .iter()
        .flatten()
        .any(|wrapped| wrapped.callee.contains(at));
    let mut deps = vec![&mut args[argument]];
    if wrapper {
        deps.push(frame);
    }
    deps
}

/// Whether code that depends on `deps` may read an imported binding: `deps` names
/// one, or a declaration of this file does that `deps` reaches, followed as far as
/// the declarations go.
fn reads_imports(decls: &[Decl], deps: &Deps) -> bool {
    if !deps.imports.is_empty() {
        return true;
    }
    let objects = deps.member_refs.iter().map(|(object, _)| object);
    let mut queue: Vec<DeclId> = deps.refs.iter().chain(objects).copied().collect();
    let mut seen: AHashSet<DeclId> = AHashSet::default();
    while let Some(decl) = queue.pop() {
        if !seen.insert(decl) {
            continue;
        }
        let Some(entry) = decls.get(decl as usize) else {
            continue;
        };
        if !entry.imports.is_empty() {
            return true;
        }
        let objects = entry.member_refs.iter().map(|(object, _)| object);
        queue.extend(entry.refs.iter().chain(objects).copied());
    }
    false
}

fn push_unique<T: PartialEq>(list: &mut Vec<T>, value: T) {
    if !list.contains(&value) {
        list.push(value);
    }
}

#[cfg(test)]
mod tests {
    use crate::module::{Callee, FineModule, ModuleAnalysis, Reading, Step, parse::analyse_source};
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
            .map(|argument| argument.deps.refs.iter().map(name).collect())
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
    fn a_require_in_an_argument_is_that_arguments_and_not_the_calls() {
        for payload in [
            "async () => require('./modal').Modal",
            "async () => { const { Modal } = require('./modal'); return Modal; }",
            "async () => (await import('./modal')).Modal",
        ] {
            let module = module(&format!(
                "import {{ createAsyncThunk }} from '@reduxjs/toolkit';
                export const t = createAsyncThunk('a/b', {payload});\n"
            ));
            let t = module.decl_named("t").unwrap() as usize;
            let call = module.decls[t].factory.as_ref().expect("a call");
            let modal = module.sources.iter().position(|s| s == "./modal").unwrap() as u32;
            let reads = |deps: &crate::module::Deps| deps.imports.iter().any(|i| i.source == modal);
            assert!(reads(&call.args[1].deps), "{payload}");
            assert!(!reads(&call.frame), "{payload}");
        }
    }

    #[test]
    fn a_name_read_by_the_callee_and_by_an_argument_is_in_both() {
        let module = module(
            "import { createAsyncThunk } from '@reduxjs/toolkit';
            const create = createAsyncThunk.withTypes<{ state: unknown }>();
            export const t = create('a/b', async () => create);\n",
        );
        let t = module.decl_named("t").unwrap() as usize;
        let create = module.decl_named("create").unwrap();
        let call = module.decls[t].factory.as_ref().expect("a call");
        assert!(call.frame.refs.contains(&create));
        assert!(call.args[1].deps.refs.contains(&create));
        assert!(!call.args[0].deps.refs.contains(&create));
    }

    #[test]
    fn a_call_a_rule_may_read_through_is_kept_for_the_graph() {
        let module = module(
            "import { createAsyncThunk } from '@reduxjs/toolkit';
            import * as rtk from '@reduxjs/toolkit';
            const create = createAsyncThunk.withTypes<{ state: unknown }>();
            const typed = rtk.createAsyncThunk.withTypes<{ state: unknown }>().withTypes();
            const other = createAsyncThunk.other();\n",
        );
        let derived = |name: &str| {
            module.decls[module.decl_named(name).unwrap() as usize]
                .derived
                .clone()
        };
        let with_types = || [Step::Prop("withTypes".to_string()), Step::Call];
        let Some(Callee::Import { name, path, .. }) = derived("create") else {
            panic!("an import");
        };
        assert_eq!(
            (name.as_str(), path.as_slice()),
            ("createAsyncThunk", &with_types()[..])
        );
        let Some(Callee::Import { name, path, .. }) = derived("typed") else {
            panic!("an import");
        };
        assert_eq!(name, "createAsyncThunk");
        assert_eq!(path, [with_types(), with_types()].concat());
        // No rule reads through `other()`, so its result is no other name for anything.
        assert_eq!(derived("other"), None);
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

    /// Whether calling the first argument of the call `store` binds, where it is
    /// written, is proven to run nothing, in a module that is `source` with the
    /// store's import above it.
    fn creator_is_quiet(source: &str) -> bool {
        let module = module(&format!("import {{ create }} from 'zustand';\n{source}"));
        let store = module.decl_named("store").expect(source) as usize;
        let call = module.decls[store].factory.as_ref().expect(source);
        call.args[0].quiet_when_called
    }

    #[test]
    fn a_creator_is_quiet_when_called_where_its_body_is_proven_to_run_nothing() {
        for source in [
            "export const store = create(() => ({ count: 0 }));",
            // The parameters may be handed on, or held by functions that call them
            // later, since no body but the creator's runs as the store is made.
            "export const store = create((set) => ({ count: 0, inc: () => set((s) => ({ count: s.count + 1 })) }));",
            "export const store = create((set, get, api) => ({ api, read: () => get() }));",
            "export const store = create(function (set) { const count = 0; return { count, set }; });",
            // What it reads is ready by the time the store is made.
            "const START = 1; export const store = create(() => ({ count: START }));",
            "import { base } from './tokens'; export const store = create(() => ({ base }));",
            "function initial() { return { count: 0 }; } export const store = create(() => initial());",
            "export const store = create(() => initial()); function initial() { return { count: 0 }; }",
        ] {
            assert!(creator_is_quiet(source), "{source}");
        }
    }

    #[test]
    fn a_creator_that_may_run_something_or_throw_when_called_is_not_quiet() {
        for source in [
            "export const store = create(() => ({ id: register() }));",
            // Calling a parameter, or reading through one, runs code nobody here
            // can see: the store's own `set` notifies its listeners.
            "export const store = create((set) => { set({ count: 1 }); return { count: 0 }; });",
            "export const store = create((set, get) => ({ count: get().count }));",
            "export const store = create((set, get, api) => ({ state: api.getState }));",
            // A `const` read before its declaration has run throws, and the creator
            // runs where the store is made.
            "export const store = create(() => ({ count: START })); const START = 1;",
            "export const store = create(() => initial()); const initial = () => ({ count: 0 });",
            // Not a function written out, or not one whose body the proof reads.
            "const creator = () => ({ count: 0 }); export const store = create(creator);",
            "export const store = create(persist(() => ({ count: 0 }), { name: 'count' }));",
            "export const store = create(async () => ({ count: 0 }));",
            "export const store = create(function* () { yield 1; });",
            "export const store = create(({ setState }) => ({ count: 0 }));",
            "export const store = create(() => this);",
        ] {
            assert!(!creator_is_quiet(source), "{source}");
        }
    }

    const MIDDLEWARE: &str = "import { create } from 'zustand';
import { immer } from 'zustand/middleware/immer';
import { combine, devtools, persist, subscribeWithSelector } from 'zustand/middleware';\n";

    /// Of the store `store` in `source`, whether evaluating the call's arguments runs
    /// nothing, which leaves the call to the graph, and whether calling its first
    /// argument is proven to run nothing.
    fn wrapped(source: &str) -> (bool, bool) {
        let module = module(source);
        let store = module.decl_named("store").expect(source);
        let deferred =
            module.conditional_init.contains(&store) && !module.init_decls.contains(&store);
        let quiet = module.decls[store as usize]
            .factory
            .as_ref()
            .is_some_and(|call| call.args[0].quiet_when_called);
        (deferred, quiet)
    }

    #[test]
    fn a_creator_wrapped_in_a_quiet_middleware_is_as_quiet_as_what_it_wraps() {
        for store in [
            "export const store = create(immer((set) => ({ count: 0, inc: () => set((s) => { s.count += 1; }) })));",
            "export const store = create(subscribeWithSelector((set) => ({ count: 0, reset: () => set({ count: 0 }) })));",
            "export const store = create(combine({ count: 0, label: 'Count' }, (set) => ({ inc: () => set((s) => ({ count: s.count + 1 })) })));",
            // Nested, and in the curried form.
            "export const store = create<{ count: number }>()(immer(subscribeWithSelector((set) => ({ count: 0 }))));",
            "export const store = create(immer(combine({ count: 0 }, (set) => ({ reset: () => set({ count: 0 }) }))));",
            "export const store = create(combine({ count: 0 }, immer((set) => ({ reset: () => set({ count: 0 }) }))));",
            "export const store = create(combine({ count: 0 }, combine({ step: 1 }, () => ({ label: 'Count' }))));",
            "export const store = create(subscribeWithSelector(function (set) { return { count: 0, set }; }));",
        ] {
            let source = format!("{MIDDLEWARE}{store}\n");
            assert_eq!(wrapped(&source), (true, true), "{store}");
        }
        // Through a namespace, and around `createStore`.
        for source in [
            "import { create } from 'zustand';\nimport * as middleware from 'zustand/middleware';\nexport const store = create(middleware.combine({ count: 0 }, () => ({ label: 'Count' })));\n",
            "import { createStore } from 'zustand/vanilla';\nimport { immer } from 'zustand/middleware/immer';\nexport const store = createStore(immer(() => ({ count: 0 })));\n",
        ] {
            assert_eq!(wrapped(source), (true, true), "{source}");
        }
    }

    #[test]
    fn a_quiet_middleware_around_a_creator_that_runs_something_is_not_quiet() {
        for (store, deferred) in [
            // Evaluating the middleware's call runs nothing, but calling what it
            // returns calls the creator.
            (
                "export const store = create(immer(() => ({ id: register() })));",
                true,
            ),
            (
                "export const store = create(subscribeWithSelector((set) => { set({ count: 1 }); return { count: 0 }; }));",
                true,
            ),
            (
                "export const store = create(combine({ count: 0 }, () => ({ id: register() })));",
                true,
            ),
            (
                "export const store = create(immer(subscribeWithSelector(() => ({ id: register() }))));",
                true,
            ),
            // `combine` evaluates its initial state as it is called, and merges it
            // into the store's with `Object.assign`, which runs every getter of it
            // and of what the creator returns.
            (
                "export const store = create(combine({ id: register() }, () => ({})));",
                false,
            ),
            (
                "export const store = create(combine({ get id() { return register(); } }, () => ({})));",
                true,
            ),
            (
                "export const store = create(combine({ count: 0 }, () => ({ get id() { return register(); } })));",
                true,
            ),
            (
                "export const store = create(combine({ count: 0 }, immer(() => ({ get id() { return register(); } }))));",
                true,
            ),
            (
                "export const store = create(combine({ count: 0 }, (set) => { if (set) return other; return {}; }));",
                true,
            ),
            // What it would merge is not written out where the proof can read it.
            (
                "const initial = { count: 0 };\nexport const store = create(combine(initial, () => ({})));",
                true,
            ),
            (
                "export const store = create(combine({ ...defaults }, () => ({})));",
                true,
            ),
            // Middleware whose effects are not proven, anywhere in the chain.
            (
                "export const store = create(immer(persist(() => ({ count: 0 }), { name: 'count' })));",
                false,
            ),
            (
                "export const store = create(devtools(immer(() => ({ count: 0 }))));",
                false,
            ),
            (
                "function logged(f) { register(); return f; }\nexport const store = create(immer(logged(() => ({ count: 0 }))));",
                false,
            ),
            // Arguments no middleware here takes.
            (
                "export const store = create(immer(() => ({ count: 0 }), { name: 'count' }));",
                true,
            ),
            // Spreading iterates, which runs code nobody here can see.
            ("export const store = create(immer(...creators));", false),
            (
                "export const store = create(combine(() => ({ count: 0 })));",
                true,
            ),
        ] {
            let source = format!("{MIDDLEWARE}{store}\n");
            assert_eq!(wrapped(&source), (deferred, false), "{store}");
        }
    }

    #[test]
    fn a_middleware_is_known_by_the_import_it_is_and_not_by_its_name() {
        for source in [
            // A function of the file, whether or not calling it is proven to run
            // nothing.
            "import { create } from 'zustand';\nfunction immer(f) { register(); return f; }\nexport const store = create(immer(() => ({ count: 0 })));\n",
            "import { create } from 'zustand';\nconst combine = (initial, f) => f;\nexport const store = create(combine({ count: 0 }, () => ({})));\n",
            // A global nobody imported.
            "import { create } from 'zustand';\nexport const store = create(subscribeWithSelector(() => ({ count: 0 })));\n",
            // Imported from a module that does not export it.
            "import { create } from 'zustand';\nimport { immer } from 'zustand/middleware';\nexport const store = create(immer(() => ({ count: 0 })));\n",
            "import { create } from 'zustand';\nimport { combine } from 'zustand';\nexport const store = create(combine({ count: 0 }, () => ({})));\n",
            "import { create } from 'zustand';\nimport { immer } from 'another-library';\nexport const store = create(immer(() => ({ count: 0 })));\n",
            // Another export of the right module under the middleware's name.
            "import { create } from 'zustand';\nimport { persist as immer } from 'zustand/middleware/immer';\nexport const store = create(immer(() => ({ count: 0 })));\n",
        ] {
            assert!(!wrapped(source).1, "{source}");
        }
        // A parameter that shares a middleware's name is the parameter, and calling
        // it is not proven.
        let source = format!(
            "{MIDDLEWARE}export const store = create((immer) => ({{ state: immer(() => ({{}})) }}));\n"
        );
        assert!(!wrapped(&source).1);
    }
}
