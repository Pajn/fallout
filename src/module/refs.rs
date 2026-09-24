//! Reference edges between declarations, and the shared-state rule.

use ahash::AHashMap;
use oxc_ast::AstKind;
use oxc_ast::ast::{
    BindingPattern, CallExpression, Expression, JSXMemberExpressionObject, UnaryOperator,
};
use oxc_semantic::{AstNodes, NodeId, SymbolId};
use oxc_span::{GetSpan, Span as OxcSpan};

use super::decls::{DeclDraft, ImportBinding, RequireCall, source_id};
use super::members::ObjectRef;
use super::parse::{Ctx, span_of};
use super::shared::{self, Access};
use super::{Decl, DeclId, ImportRef, ImportTarget, SourceId, Span};

/// The edges the shared-state rule added, by the declaration each was added to.
pub(crate) type SharedEdges = AHashMap<DeclId, Vec<DeclId>>;

/// Fills in each declaration's references, and returns the import statement spans
/// paired with the declarations that use the bindings those statements introduce.
///
/// A read of one property of an object in `objects` is a reference to that
/// property rather than to the declaration. See [`writes_object`] for when a use
/// of one counts as a write.
///
/// Also returns the edges the shared-state rule added, by the declaration they
/// were added to, since those belong to every property of an object declaration
/// whatever each property names.
pub(crate) fn link(
    ctx: &Ctx<'_>,
    drafts: &[DeclDraft],
    imports: &[ImportBinding],
    objects: &AHashMap<SymbolId, ObjectRef>,
    decls: &mut [Decl],
) -> (Vec<(Span, Vec<DeclId>)>, SharedEdges) {
    let scoping = ctx.semantic.scoping();
    let root = scoping.root_scope_id();

    let statement_decls = statement_decls(drafts);

    let decl_by_name: AHashMap<&str, DeclId> = drafts
        .iter()
        .enumerate()
        .map(|(id, draft)| (draft.name.as_str(), id as DeclId))
        .collect();
    let import_by_name: AHashMap<&str, &ImportBinding> = imports
        .iter()
        .map(|binding| (binding.local.as_str(), binding))
        .collect();

    // How each declaration touches each module-scope binding of this file, for the
    // shared-state rule.
    let mut accesses_of: AHashMap<SymbolId, Vec<(DeclId, Access)>> = AHashMap::default();
    // Declarations referencing each import statement's bindings, for hunk attribution.
    let mut import_users: AHashMap<Span, Vec<DeclId>> = AHashMap::default();

    for symbol_id in scoping.symbol_ids() {
        if scoping.symbol_scope_id(symbol_id) != root {
            continue;
        }
        let name = scoping.symbol_name(symbol_id);
        let target_decl = decl_by_name.get(name).copied();
        let target_import = import_by_name.get(name).copied();
        let object = objects.get(&symbol_id);
        // Only a binding declared here can carry state between two declarations
        // here. Of an object read only for its members, calling a member that
        // cannot reach the object through `this` is a read of that member; what is
        // done to a member's value still counts.
        let shared = target_decl.is_some_and(|decl| !drafts[decl as usize].immutable);
        let mode = if let Some(object) = object {
            shared::Mode::Members(&object.callable)
        } else if shared && shared::independent_properties(ctx, symbol_id) {
            shared::Mode::Properties
        } else {
            shared::Mode::Whole
        };

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

            let member = object.and_then(|_| unwritten_member_read(ctx.semantic.nodes(), node_id));
            for &user in users {
                if let Some(target) = target_decl {
                    // A declaration referring to itself is not an edge.
                    if user != target {
                        match &member {
                            Some(member) => {
                                let read = (target, member.clone());
                                if !decls[user as usize].member_refs.contains(&read) {
                                    decls[user as usize].member_refs.push(read);
                                }
                            }
                            None => push_unique(&mut decls[user as usize].refs, target),
                        }
                    }
                }
                if let Some(binding) = target_import {
                    let reference = narrowed(ctx.semantic.nodes(), node_id, &binding.reference);
                    push_import(&mut decls[user as usize].imports, reference);
                    push_unique(import_users.entry(binding.span).or_default(), user);
                }
            }

            if !shared {
                continue;
            }
            // An access an alias carried elsewhere belongs to the declaration it is
            // written in, which is where the write happens.
            for (offset, access) in shared::accesses(ctx, node_id, mode) {
                let Some(users) = ctx
                    .statement_at(offset)
                    .and_then(|statement| statement_decls.get(&statement))
                else {
                    continue;
                };
                let entry = accesses_of.entry(symbol_id).or_default();
                for &user in users {
                    let access = (user, access.clone());
                    if !entry.contains(&access) {
                        entry.push(access);
                    }
                }
            }
        }
    }

    let shared = apply_shared_state(&accesses_of, decls);

    let mut spans: Vec<(Span, Vec<DeclId>)> = import_users.into_iter().collect();
    spans.sort_by_key(|(span, _)| *span);
    (spans, shared)
}

/// Whether a reference could change what its binding holds, for the shared-state
/// rule.
///
/// For an object read only for its members, a call on one of them, `utils.fn()`,
/// leaves the object as it was where the member is known not to reach the object
/// through `this`. Anything else the whole-value rule counts as a write still is
/// one: handing a member to other code, `wipe(store.items)`, can change what it
/// holds.
pub(crate) fn writes_object(
    nodes: &AstNodes<'_>,
    node_id: NodeId,
    object: Option<&ObjectRef>,
) -> bool {
    if classify(nodes, node_id) != Use::Mutate {
        return false;
    }
    let Some(object) = object else {
        return true;
    };
    // `export default utils` hands the object to its importers, whose own reads
    // and writes are theirs to account for, as an `export { }` list does.
    if matches!(
        nodes.parent_kind(node_id),
        AstKind::ExportDefaultDeclaration(_)
    ) {
        return false;
    }
    !member_call(nodes, node_id).is_some_and(|member| object.callable.contains(&member))
}

/// The member a reference is the object of a direct call on: `utils.fn()`, and
/// `(utils.fn)()`, which binds `this` the same way.
fn member_call(nodes: &AstNodes<'_>, node_id: NodeId) -> Option<String> {
    let (inner, span) = unwrapped(nodes, node_id);
    let AstKind::StaticMemberExpression(member) = nodes.parent_kind(inner) else {
        return None;
    };
    if member.object.span() != span {
        return None;
    }
    let (outer, outer_span) = unwrapped(nodes, nodes.parent_id(inner));
    match nodes.parent_kind(outer) {
        AstKind::CallExpression(call) if call.callee.span() == outer_span => {
            Some(member.property.name.to_string())
        }
        _ => None,
    }
}

/// A declaration that could change what a shared module-scope binding holds is
/// reachable from every other declaration that reads it: that is how an edit to one
/// travels to the other without either naming the other.
///
/// Only the writers pull. Two declarations that merely read the same binding cannot
/// affect each other through it, which is what keeps sibling components apart in
/// code where every one of them calls the same helper.
///
/// Only a binding declared in this file holds a module-scope value that two
/// declarations here could pass between them. An imported binding is the exporting
/// module's business, and is already an edge to that module. Of an object whose
/// properties are independent, a write reaches only the uses of the property it
/// writes, and the uses of the object as a whole; see [`shared`].
fn apply_shared_state(
    accesses_of: &AHashMap<SymbolId, Vec<(DeclId, Access)>>,
    decls: &mut [Decl],
) -> SharedEdges {
    let mut added = SharedEdges::default();
    for accesses in accesses_of.values() {
        for (user, used) in accesses {
            for (writer, written) in accesses {
                if user != writer && used.sees(written) {
                    push_unique(&mut decls[*user as usize].refs, *writer);
                    push_unique(added.entry(*user).or_default(), *writer);
                }
            }
        }
    }
    added
}

/// A namespace read for one of its exports depends on that export alone, and an
/// export read for one of its properties depends on that property alone.
///
/// `import * as ns from "./g"` gives the whole export table of `g`, and most uses
/// of it immediately pick one name back out. Every other shape — passing `ns`
/// somewhere, a computed `ns[key]` — keeps the whole table, and a reference that
/// does both contributes both, so the wide edge is never lost by accident.
///
/// A property is taken off an export only where nothing is written through it:
/// `utils.x = other` changes the object every other reader of `utils` sees.
pub(crate) fn narrowed(nodes: &AstNodes<'_>, node_id: NodeId, reference: &ImportRef) -> ImportRef {
    let target = match &reference.target {
        ImportTarget::Namespace => match member_read(nodes, node_id) {
            Some(export) => export_read(nodes, node_id, export),
            None => return reference.clone(),
        },
        ImportTarget::Named(export) => match unwritten_member_read(nodes, node_id) {
            Some(member) => ImportTarget::Member {
                export: export.clone(),
                member,
            },
            None => return reference.clone(),
        },
        ImportTarget::Member { .. } => return reference.clone(),
    };
    ImportRef {
        source: reference.source,
        target,
    }
}

/// `export`, read off the module object at `object`, narrowed to one property of it
/// where the read goes on to pick one: `ns.utils.formatDate` reads `formatDate` of
/// `utils`. A write through the property keeps the whole export.
fn export_read(nodes: &AstNodes<'_>, object: NodeId, export: String) -> ImportTarget {
    let read = nodes.parent_id(unwrapped(nodes, object).0);
    match unwritten_member_read(nodes, read) {
        Some(member) => ImportTarget::Member { export, member },
        None => ImportTarget::Named(export),
    }
}

/// What a binding destructured out of a module object reads of `export`: one
/// property at a time where every use is a read of one and the binding stays in
/// this file, and the whole export otherwise.
fn binding_reads(ctx: &Ctx<'_>, pattern: &BindingPattern<'_>, export: String) -> Vec<ImportTarget> {
    let whole = || vec![ImportTarget::Named(export.clone())];
    let BindingPattern::BindingIdentifier(identifier) = pattern else {
        return whole();
    };
    let Some(symbol_id) = identifier.symbol_id.get() else {
        return whole();
    };
    let scoping = ctx.semantic.scoping();
    let nodes = ctx.semantic.nodes();
    // `export const { utils } = require("./u")` hands the whole value on.
    let declared = scoping.symbol_declaration(symbol_id);
    let declarator = std::iter::once(declared)
        .chain(nodes.ancestor_ids(declared))
        .find(|&id| matches!(nodes.kind(id), AstKind::VariableDeclarator(_)));
    if let Some(declarator) = declarator {
        let declaration = nodes.parent_id(declarator);
        if matches!(
            nodes.parent_kind(declaration),
            AstKind::ExportDeclaration(_)
        ) {
            return whole();
        }
    }
    let mut targets = Vec::new();
    for reference_id in scoping.get_resolved_reference_ids(symbol_id) {
        let reference = scoping.get_reference(*reference_id);
        if reference.is_write() {
            return whole();
        }
        let Some(member) = unwritten_member_read(nodes, reference.node_id()) else {
            return whole();
        };
        let target = ImportTarget::Member {
            export: export.clone(),
            member,
        };
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    // An unused binding still names the export it was taken from.
    if targets.is_empty() { whole() } else { targets }
}

/// Walks out through any parentheses, returning the outermost node standing for the
/// same value and its span. `(await import("./g")).x` puts one between the import
/// and the member read, and without this the read is not recognised.
fn unwrapped(nodes: &AstNodes<'_>, node_id: NodeId) -> (NodeId, OxcSpan) {
    let mut current = node_id;
    let mut span = nodes.get_node(node_id).kind().span();
    while let AstKind::ParenthesizedExpression(paren) = nodes.parent_kind(current) {
        span = paren.span;
        current = nodes.parent_id(current);
    }
    (current, span)
}

/// The property read straight off this reference: `ns.x`, and `<ns.X />`.
fn member_read(nodes: &AstNodes<'_>, node_id: NodeId) -> Option<String> {
    let (node_id, span) = unwrapped(nodes, node_id);
    match nodes.parent_kind(node_id) {
        AstKind::StaticMemberExpression(member) if member.object.span() == span => {
            Some(member.property.name.to_string())
        }
        // `<ns.Thing />`. Only the innermost object is the binding; `<a.b.c />`
        // reads `b` off `a`, and what happens after that is `b`'s business.
        AstKind::JSXMemberExpression(member)
            if matches!(&member.object, JSXMemberExpressionObject::IdentifierReference(ident)
                if ident.span == span) =>
        {
            Some(member.property.name.to_string())
        }
        _ => None,
    }
}

/// How a declaration touches a binding.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Use {
    /// Cannot change what the binding holds.
    Read,
    /// Could, or we cannot tell.
    Mutate,
}

/// Reads the shape a reference is written in.
///
/// The listed shapes leave the binding as they found it. Everything else — passing
/// it somewhere, returning it, assigning through it, spreading it — hands the value
/// to code this declaration does not contain, so it counts as a write.
pub(crate) fn classify(nodes: &AstNodes<'_>, node_id: NodeId) -> Use {
    let span = nodes.get_node(node_id).kind().span();
    match nodes.parent_kind(node_id) {
        // `typeof s` reads nothing that can be written back.
        AstKind::UnaryExpression(unary) if unary.operator == UnaryOperator::Typeof => Use::Read,
        // Calling or constructing does not rebind the name. A helper that mutates
        // itself is not covered, which is the assumption every bundler makes.
        AstKind::CallExpression(call) if call.callee.span() == span => Use::Read,
        AstKind::NewExpression(new) if new.callee.span() == span => Use::Read,
        // `<S />` and `<S>...</S>`, whose closing tag names it again. Rendering a
        // component passes props to the component, which is its own declaration.
        //
        // `<S.Provider value={...}>` is deliberately not here. Naming a member of a
        // binding as an element is a call on that member, and it is how a React
        // context is written to: the provider puts a value in, every consumer of the
        // same context reads it out. That is a channel between two declarations
        // however little the syntax looks like one.
        AstKind::JSXOpeningElement(_) | AstKind::JSXClosingElement(_) => Use::Read,
        // `s.x` is a read where the value it yields stays in the expression.
        AstKind::StaticMemberExpression(_) | AstKind::ComputedMemberExpression(_) => {
            match nodes.parent_kind(nodes.parent_id(node_id)) {
                AstKind::AssignmentExpression(_)
                | AstKind::UpdateExpression(_)
                | AstKind::CallExpression(_)
                | AstKind::NewExpression(_)
                | AstKind::SpreadElement(_) => Use::Mutate,
                AstKind::UnaryExpression(unary) if unary.operator == UnaryOperator::Delete => {
                    Use::Mutate
                }
                _ => Use::Read,
            }
        }
        _ => Use::Mutate,
    }
}

/// Attributes every `require("./x")` to the declarations of the statement it is
/// written in, and returns the targets of the calls that belong to no declaration.
///
/// Static property reads and destructuring select individual exports. Escaping or
/// mutable module objects retain the whole namespace. A call in a statement that
/// declares nothing runs on evaluation, so its target joins the bare sources.
pub(crate) fn attach_requires(
    ctx: &Ctx<'_>,
    drafts: &[DeclDraft],
    requires: &[RequireCall],
    decls: &mut [Decl],
) -> Vec<SourceId> {
    if requires.is_empty() {
        return Vec::new();
    }
    let statement_decls = statement_decls(drafts);
    let mut init_sources = Vec::new();

    let call_nodes: AHashMap<Span, NodeId> = ctx
        .semantic
        .nodes()
        .iter_enumerated()
        .filter_map(|(id, node)| {
            matches!(node.kind(), AstKind::CallExpression(_))
                .then_some((span_of(node.kind().span()), id))
        })
        .collect();

    for call in requires {
        let users = ctx
            .statement_at(call.span.start)
            .and_then(|statement| statement_decls.get(&statement));

        let Some(users) = users else {
            if !init_sources.contains(&call.source) {
                init_sources.push(call.source);
            }
            continue;
        };

        let targets = call_nodes
            .get(&call.span)
            .map(|&node_id| require_targets(ctx, node_id))
            .unwrap_or_else(|| vec![ImportTarget::Namespace]);
        for target in targets {
            for &user in users {
                push_import(
                    &mut decls[user as usize].imports,
                    ImportRef {
                        source: call.source,
                        target: target.clone(),
                    },
                );
            }
        }
    }

    init_sources
}

/// Narrow only the runtime require: a local function with that name can return
/// something unrelated to the named module, so the old wide dependency remains.
fn require_targets(ctx: &Ctx<'_>, node_id: NodeId) -> Vec<ImportTarget> {
    let nodes = ctx.semantic.nodes();
    let AstKind::CallExpression(call) = nodes.get_node(node_id).kind() else {
        return vec![ImportTarget::Namespace];
    };
    let Expression::Identifier(callee) = &call.callee else {
        return vec![ImportTarget::Namespace];
    };
    if callee.reference_id.get().is_none_or(|id| {
        ctx.semantic
            .scoping()
            .get_reference(id)
            .symbol_id()
            .is_some()
    }) {
        return vec![ImportTarget::Namespace];
    }
    let (node_id, span) = unwrapped(nodes, node_id);
    let targets = match nodes.parent_kind(node_id) {
        AstKind::VariableDeclarator(declarator)
            if declarator
                .init
                .as_ref()
                .is_some_and(|init| init.span() == span) =>
        {
            // Exporting the object itself is an escape even if its local uses
            // all select members. Destructuring exports only selected values.
            let declaration = nodes.parent_id(nodes.parent_id(node_id));
            if matches!(&declarator.id, BindingPattern::BindingIdentifier(_))
                && matches!(
                    nodes.parent_kind(declaration),
                    AstKind::ExportDeclaration(_)
                )
            {
                return vec![ImportTarget::Namespace];
            }
            require_bound_targets(ctx, &declarator.id)
        }
        _ => unwritten_member_read(nodes, node_id)
            .map(|name| vec![export_read(nodes, node_id, name)])
            .unwrap_or_else(|| vec![ImportTarget::Namespace]),
    };
    // Even an unused binding or empty pattern evaluates the module. Keeping the
    // namespace carries this edge under inline_requires, including empty modules.
    if targets.is_empty() {
        vec![ImportTarget::Namespace]
    } else {
        targets
    }
}

fn require_bound_targets(ctx: &Ctx<'_>, pattern: &BindingPattern<'_>) -> Vec<ImportTarget> {
    let BindingPattern::BindingIdentifier(identifier) = pattern else {
        return bound_targets(ctx, pattern);
    };
    let Some(symbol_id) = identifier.symbol_id.get() else {
        return vec![ImportTarget::Namespace];
    };
    let scoping = ctx.semantic.scoping();
    let mut targets = Vec::new();
    for reference_id in scoping.get_resolved_reference_ids(symbol_id) {
        let reference = scoping.get_reference(*reference_id);
        if reference.is_write() {
            return vec![ImportTarget::Namespace];
        }
        let Some(name) = unwritten_member_read(ctx.semantic.nodes(), reference.node_id()) else {
            return vec![ImportTarget::Namespace];
        };
        let target = export_read(ctx.semantic.nodes(), reference.node_id(), name);
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets
}

/// Writing through an object or deleting a member can change the export table.
/// Walk the full member chain, through type-only wrappers, so `ns.x.y = value` and
/// `ns.x! = value` also keep the broad edge.
pub(crate) fn unwritten_member_read(nodes: &AstNodes<'_>, node_id: NodeId) -> Option<String> {
    let name = member_read(nodes, node_id)?;
    let (mut current, _) = unwrapped(nodes, node_id);
    loop {
        match nodes.parent_kind(current) {
            AstKind::StaticMemberExpression(_)
            | AstKind::ComputedMemberExpression(_)
            | AstKind::ParenthesizedExpression(_)
            | AstKind::ChainExpression(_)
            | AstKind::TSNonNullExpression(_)
            | AstKind::TSAsExpression(_)
            | AstKind::TSSatisfiesExpression(_)
            | AstKind::TSTypeAssertion(_) => {
                current = nodes.parent_id(current);
            }
            AstKind::AssignmentExpression(_)
            | AstKind::UpdateExpression(_)
            | AstKind::AssignmentTargetPropertyIdentifier(_)
            | AstKind::AssignmentTargetPropertyProperty(_)
            | AstKind::ArrayAssignmentTarget(_)
            | AstKind::AssignmentTargetRest(_)
            | AstKind::AssignmentTargetWithDefault(_)
            | AstKind::ForInStatement(_)
            | AstKind::ForOfStatement(_) => return None,
            AstKind::UnaryExpression(unary) if unary.operator == UnaryOperator::Delete => {
                return None;
            }
            _ => return Some(name),
        }
    }
}

/// Attributes every `import("./x")` to the declarations of the statement it is
/// written in, and returns the targets of the ones that belong to no declaration.
///
/// A dynamic import yields the target's whole export table, exactly as `import * as`
/// does, so the same narrowing applies: where the module object is immediately read
/// for one name, only that export is a dependency.
///
/// `None` if a specifier is missing from `sources`, which coarsens the module rather
/// than leaving the dependency unrecorded.
pub(crate) fn attach_dynamic_imports(
    ctx: &Ctx<'_>,
    drafts: &[DeclDraft],
    sources: &[String],
    decls: &mut [Decl],
) -> Option<Vec<SourceId>> {
    let statement_decls = statement_decls(drafts);
    let mut init_sources = Vec::new();

    for (node_id, node) in ctx.semantic.nodes().iter_enumerated() {
        let AstKind::ImportExpression(expression) = node.kind() else {
            continue;
        };
        // The Coarsener has already rejected a computed specifier, so anything
        // reaching here names its module in a plain string.
        let Expression::StringLiteral(literal) = &expression.source else {
            continue;
        };
        let source = source_id(sources, literal.value.as_str())?;

        let users = ctx
            .statement_at(expression.span.start)
            .and_then(|statement| statement_decls.get(&statement));

        let Some(users) = users else {
            if !init_sources.contains(&source) {
                init_sources.push(source);
            }
            continue;
        };

        for target in dynamic_targets(ctx, node_id) {
            for &user in users {
                push_import(
                    &mut decls[user as usize].imports,
                    ImportRef {
                        source,
                        target: target.clone(),
                    },
                );
            }
        }
    }

    Some(init_sources)
}

/// What a dynamic import's module object is read for, at the two places the object
/// becomes reachable: after an `await`, and as the argument of `.then`.
fn dynamic_targets(ctx: &Ctx<'_>, node_id: NodeId) -> Vec<ImportTarget> {
    let nodes = ctx.semantic.nodes();
    let (node_id, span) = unwrapped(nodes, node_id);

    match nodes.parent_kind(node_id) {
        AstKind::AwaitExpression(_) => {
            let (awaited, _) = unwrapped(nodes, nodes.parent_id(node_id));
            match nodes.parent_kind(awaited) {
                AstKind::VariableDeclarator(declarator) => bound_targets(ctx, &declarator.id),
                // `(await import("./g")).x`
                AstKind::StaticMemberExpression(member) => {
                    vec![export_read(
                        nodes,
                        awaited,
                        member.property.name.to_string(),
                    )]
                }
                _ => vec![ImportTarget::Namespace],
            }
        }
        // `import("./g").then(m => m.x)`
        AstKind::StaticMemberExpression(member)
            if member.property.name == "then" && member.object.span() == span =>
        {
            let member_id = nodes.parent_id(node_id);
            match nodes.parent_kind(member_id) {
                AstKind::CallExpression(call) => callback_targets(ctx, call),
                _ => vec![ImportTarget::Namespace],
            }
        }
        _ => vec![ImportTarget::Namespace],
    }
}

/// The module object handed to `.then(...)`, read through the callback's parameter.
fn callback_targets(ctx: &Ctx<'_>, call: &CallExpression<'_>) -> Vec<ImportTarget> {
    let Some(argument) = call.arguments.first().and_then(|a| a.as_expression()) else {
        return vec![ImportTarget::Namespace];
    };
    let params = match argument {
        Expression::ArrowFunctionExpression(arrow) => &arrow.params,
        Expression::FunctionExpression(function) => &function.params,
        _ => return vec![ImportTarget::Namespace],
    };
    match params.items.first() {
        // A callback that ignores the module depends on none of its exports.
        None => Vec::new(),
        Some(first) => bound_targets(ctx, &first.pattern),
    }
}

/// What the pattern a module object is bound to reads out of it.
fn bound_targets(ctx: &Ctx<'_>, pattern: &BindingPattern<'_>) -> Vec<ImportTarget> {
    match pattern {
        BindingPattern::BindingIdentifier(identifier) => {
            let Some(symbol_id) = identifier.symbol_id.get() else {
                return vec![ImportTarget::Namespace];
            };
            let scoping = ctx.semantic.scoping();
            let mut targets = Vec::new();
            for reference_id in scoping.get_resolved_reference_ids(symbol_id) {
                let node_id = scoping.get_reference(*reference_id).node_id();
                match member_read(ctx.semantic.nodes(), node_id) {
                    Some(name) => targets.push(export_read(ctx.semantic.nodes(), node_id, name)),
                    // Used as a whole somewhere, so the whole table is a dependency.
                    None => return vec![ImportTarget::Namespace],
                }
            }
            targets
        }
        // `const { a, b } = await import("./g")` names its exports outright.
        BindingPattern::ObjectPattern(object) => {
            if object.rest.is_some() {
                return vec![ImportTarget::Namespace];
            }
            let mut targets = Vec::new();
            for property in &object.properties {
                let Some(name) = (!property.computed)
                    .then(|| property.key.static_name())
                    .flatten()
                else {
                    return vec![ImportTarget::Namespace];
                };
                for target in binding_reads(ctx, &property.value, name.to_string()) {
                    if !targets.contains(&target) {
                        targets.push(target);
                    }
                }
            }
            targets
        }
        _ => vec![ImportTarget::Namespace],
    }
}

/// Which declarations does each top-level statement introduce?
fn statement_decls(drafts: &[DeclDraft]) -> AHashMap<usize, Vec<DeclId>> {
    let mut map: AHashMap<usize, Vec<DeclId>> = AHashMap::default();
    for (id, draft) in drafts.iter().enumerate() {
        map.entry(draft.statement).or_default().push(id as DeclId);
    }
    map
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

#[cfg(test)]
mod tests {
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::{GetSpan, SourceType};

    use super::{Use, classify};

    fn require_targets_of(source: &str) -> Vec<super::ImportTarget> {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source, SourceType::tsx()).parse();
        assert!(
            parsed.diagnostics.is_empty(),
            "{source}: {:?}",
            parsed.diagnostics
        );
        let built = SemanticBuilder::new()
            .with_build_nodes(true)
            .build(&parsed.program);
        let ctx = super::Ctx {
            semantic: &built.semantic,
            statements: Vec::new(),
        };
        let node_id = ctx
            .semantic
            .nodes()
            .iter_enumerated()
            .find_map(|(id, node)| {
                if let oxc_ast::AstKind::CallExpression(call) = node.kind()
                    && super::super::parse::is_require(call)
                {
                    Some(id)
                } else {
                    None
                }
            })
            .expect("require call");
        super::require_targets(&ctx, node_id)
    }

    /// What `page` imports, as targets.
    fn page_imports(source: &str) -> Vec<super::ImportTarget> {
        use crate::module::{ModuleAnalysis, Reading, parse::analyse_source};
        let (ModuleAnalysis::Fine(module), _) = analyse_source(
            std::path::Path::new("page.tsx"),
            source,
            &Reading::default(),
        )
        .unwrap() else {
            panic!("expected fine module: {source}")
        };
        let page = module.decl_named("page").expect("page");
        module.decls[page as usize]
            .imports
            .iter()
            .map(|import| import.target.clone())
            .collect()
    }

    #[test]
    fn a_property_read_off_an_export_names_the_property() {
        let member = |export: &str, member: &str| super::ImportTarget::Member {
            export: export.into(),
            member: member.into(),
        };
        for (source, expected) in [
            (
                "import { utils } from './u'; export const page = () => utils.format();",
                member("utils", "format"),
            ),
            (
                "import { utils } from './u'; export const page = () => <utils.Button />;",
                member("utils", "Button"),
            ),
            (
                "import * as ns from './u'; export const page = () => ns.utils.format();",
                member("utils", "format"),
            ),
            (
                "import * as ns from './u'; export const page = () => (ns).utils.format();",
                member("utils", "format"),
            ),
            (
                "import utils from './u'; export const page = () => utils.format;",
                member("default", "format"),
            ),
        ] {
            assert_eq!(page_imports(source), [expected], "{source}");
        }
    }

    #[test]
    fn a_property_read_through_require_or_import_names_the_property() {
        let member = || super::ImportTarget::Member {
            export: "utils".into(),
            member: "format".into(),
        };
        for source in [
            "export const page = () => require('./u').utils.format();",
            "const u = require('./u'); export const page = () => u.utils.format();",
            "const { utils } = require('./u'); export const page = () => utils.format();",
            "const { utils: renamed } = require('./u'); export const page = () => renamed.format();",
            "export const page = async () => (await import('./u')).utils.format();",
            "export const page = async () => { const u = await import('./u'); return u.utils.format(); };",
            "export const page = async () => { const { utils } = await import('./u'); return utils.format(); };",
            "export const page = () => import('./u').then((u) => u.utils.format());",
        ] {
            let module = crate::module::parse::analyse_source(
                std::path::Path::new("page.tsx"),
                source,
                &crate::module::Reading::default(),
            )
            .unwrap()
            .0;
            let module = module.as_fine().expect("fine");
            let imports: Vec<_> = module
                .decls
                .iter()
                .flat_map(|decl| decl.imports.iter().map(|i| i.target.clone()))
                .collect();
            assert_eq!(imports, [member()], "{source}");
        }
    }

    #[test]
    fn a_destructured_export_that_escapes_is_taken_whole() {
        for source in [
            "export const { utils } = require('./u'); utils.format();",
            "const { utils } = require('./u'); export { utils }; utils.format();",
            "const { utils } = require('./u'); use(utils);",
            "let { utils } = require('./u'); utils = other; utils.format();",
            "const { utils } = require('./u'); utils.format = other;",
        ] {
            assert_eq!(
                require_targets_of(source),
                [super::ImportTarget::Named("utils".into())],
                "{source}"
            );
        }
    }

    #[test]
    fn an_export_used_or_written_whole_is_not_narrowed_to_a_property() {
        let named = |name: &str| super::ImportTarget::Named(name.into());
        for (source, expected) in [
            (
                "import { utils } from './u'; export const page = () => use(utils);",
                named("utils"),
            ),
            (
                "import { utils } from './u'; export const page = () => utils[key];",
                named("utils"),
            ),
            (
                "import { utils } from './u'; export const page = () => { utils.format = other; };",
                named("utils"),
            ),
            (
                "import { utils } from './u'; export const page = () => { delete utils.format; };",
                named("utils"),
            ),
            (
                "import * as ns from './u'; export const page = () => { ns.utils.format = other; };",
                named("utils"),
            ),
            (
                "import * as ns from './u'; export const page = () => use(ns.utils);",
                named("utils"),
            ),
        ] {
            assert_eq!(page_imports(source), [expected], "{source}");
        }
    }

    #[test]
    fn require_selects_static_members_and_patterns() {
        for source in [
            "const value = require('./g').x;",
            "const value = (require('./g')).x();",
            "const { x: renamed = fallback } = require('./g');",
            "const { x: { nested } } = require('./g');",
            "const m = require('./g'); export const read = () => m.x();",
            "function read() { const m = require('./g'); return m.x; }",
            "export function read() { const m = require('./g'); return m.x; }",
        ] {
            assert_eq!(
                require_targets_of(source),
                [super::ImportTarget::Named("x".into())],
                "{source}"
            );
        }
        assert_eq!(
            require_targets_of("const { x, y } = require('./g');"),
            [
                super::ImportTarget::Named("x".into()),
                super::ImportTarget::Named("y".into())
            ]
        );
    }

    #[test]
    fn require_keeps_unsafe_and_evaluation_only_objects_wide() {
        for source in [
            "const m = require('./g'); consume(m);",
            "const m = require('./g'); const alias = m; alias.x;",
            "const m = require('./g'); m[key];",
            "const { [key]: x } = require('./g');",
            "const { x, ...rest } = require('./g');",
            "let m = require('./g'); m = other; m.x;",
            "const m = require('./g'); m.x = other;",
            "const m = require('./g'); m.x.y = other;",
            "const m = require('./g'); m.x! = other;",
            "const m = require('./g'); (m.x as any) = other;",
            "const m = require('./g'); (m.x satisfies any) = other;",
            "const m = require('./g'); m.x!.y = other;",
            "const m = require('./g'); delete m.x;",
            "const m = require('./g'); m.x++;",
            "const m = require('./g'); [m.x] = other;",
            "const m = require('./g'); ({ value: m.x } = other);",
            "const m = require('./g'); for (m.x of values) {}",
            "const m = require('./g'); for (m.x in values) {}",
            "export const m = require('./g'); m.x;",
            "const m = require('./g'); export { m }; m.x;",
            "function f(require) { return require('./g').x; }",
            "const m = require('./g');",
            "const {} = require('./g');",
            "const value = require('./g')[key];",
        ] {
            assert_eq!(
                require_targets_of(source),
                [super::ImportTarget::Namespace],
                "{source}"
            );
        }
    }

    /// How every reference to `S` in `body` is read, in source order.
    fn uses_of_s(body: &str) -> Vec<Use> {
        let source = format!("const S = make();\n{body}\n");
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, &source, SourceType::tsx()).parse();
        let semantic = SemanticBuilder::new()
            .with_build_nodes(true)
            .build(&parsed.program)
            .semantic;
        let scoping = semantic.scoping();

        let mut found: Vec<(u32, Use)> = Vec::new();
        for symbol_id in scoping.symbol_ids() {
            if scoping.symbol_name(symbol_id) != "S" {
                continue;
            }
            for reference_id in scoping.get_resolved_reference_ids(symbol_id) {
                let node_id = scoping.get_reference(*reference_id).node_id();
                let start = semantic.nodes().get_node(node_id).kind().span().start;
                found.push((start, classify(semantic.nodes(), node_id)));
            }
        }
        found.sort_by_key(|(start, _)| *start);
        found.into_iter().map(|(_, how)| how).collect()
    }

    #[test]
    fn using_a_binding_without_touching_it_is_a_read() {
        assert_eq!(uses_of_s("export const a = () => S(1);"), [Use::Read]);
        assert_eq!(uses_of_s("export const a = () => new S(2);"), [Use::Read]);
        assert_eq!(uses_of_s("export const a = () => typeof S;"), [Use::Read]);
        assert_eq!(uses_of_s("export const a = () => <S />;"), [Use::Read]);
        // A closing tag names the component a second time.
        assert_eq!(
            uses_of_s("export const a = () => <S>x</S>;"),
            [Use::Read, Use::Read]
        );
    }

    #[test]
    fn an_element_naming_a_member_is_a_call_on_it() {
        // `<Ctx.Provider value={v}>` writes v into the context every consumer of
        // `Ctx` reads. Compound components such as `<Menu.Item />` only read, and
        // are widened along with it.
        assert_eq!(
            uses_of_s("export const a = () => <S.Item />;"),
            [Use::Mutate]
        );
        assert_eq!(
            uses_of_s("export const a = () => <S.Provider value={1}>x</S.Provider>;"),
            [Use::Mutate, Use::Mutate]
        );
    }

    #[test]
    fn a_property_read_that_stays_in_the_expression_is_a_read() {
        assert_eq!(uses_of_s("export const a = () => S.x;"), [Use::Read]);
        assert_eq!(uses_of_s("export const a = () => S[\"y\"];"), [Use::Read]);
        assert_eq!(uses_of_s("export const a = () => S.x + 1;"), [Use::Read]);
    }

    #[test]
    fn writing_through_a_binding_is_a_mutation() {
        assert_eq!(
            uses_of_s("export const a = () => { S.x = 1 };"),
            [Use::Mutate]
        );
        assert_eq!(uses_of_s("export const a = () => S++;"), [Use::Mutate]);
        assert_eq!(
            uses_of_s("export const a = () => { delete S.x };"),
            [Use::Mutate]
        );
    }

    #[test]
    fn handing_the_value_to_other_code_is_a_mutation() {
        // The callee is the reference in `S(1)`; here `S` is an argument, and the
        // function it lands in can do anything with it.
        assert_eq!(uses_of_s("export const a = () => other(S);"), [Use::Mutate]);
        // A method call is the ordinary way to mutate: `.push`, `.set`, `.add`.
        assert_eq!(
            uses_of_s("export const a = () => S.push(1);"),
            [Use::Mutate]
        );
        // A property read can escape the same way the binding itself can.
        assert_eq!(
            uses_of_s("export const a = () => other(S.x);"),
            [Use::Mutate]
        );
        assert_eq!(uses_of_s("export const a = () => [...S];"), [Use::Mutate]);
        // Returning the binding hands it to the caller.
        assert_eq!(uses_of_s("export const a = () => S;"), [Use::Mutate]);
    }

    #[test]
    fn each_reference_is_read_on_its_own() {
        assert_eq!(
            uses_of_s("export const a = () => { other(S); return S.y };"),
            [Use::Mutate, Use::Read]
        );
    }

    #[test]
    fn a_property_read_beside_an_assignment_is_not_told_apart() {
        // `S.y` here only reads. Working out which side of the assignment a member
        // expression sits on would narrow this, and narrowing is the direction that
        // can be wrong, so both sides count as writes.
        assert_eq!(
            uses_of_s("export const a = () => { S.x = S.y };"),
            [Use::Mutate, Use::Mutate]
        );
    }
}
