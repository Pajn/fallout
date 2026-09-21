//! Reference edges between declarations, and the shared-state rule.

use ahash::AHashMap;
use oxc_ast::AstKind;
use oxc_ast::ast::UnaryOperator;
use oxc_semantic::{AstNodes, NodeId, SymbolId};
use oxc_span::GetSpan;

use super::decls::{DeclDraft, ImportBinding, RequireCall};
use super::parse::{Ctx, span_of};
use super::{Decl, DeclId, ImportRef, ImportTarget, SourceId, Span};

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

    // Declarations referencing each module-scope binding, for the shared-state rule,
    // and the subset whose reference could change what the binding holds.
    let mut users_of: AHashMap<SymbolId, Vec<DeclId>> = AHashMap::default();
    let mut mutators_of: AHashMap<SymbolId, Vec<DeclId>> = AHashMap::default();
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
                if classify(ctx.semantic.nodes(), node_id) == Use::Mutate {
                    push_unique(mutators_of.entry(symbol_id).or_default(), user);
                }
            }
        }
    }

    apply_shared_state(drafts, &decl_by_name, &users_of, &mutators_of, ctx, decls);

    let mut spans: Vec<(Span, Vec<DeclId>)> = import_users.into_iter().collect();
    spans.sort_by_key(|(span, _)| *span);
    spans
}

/// A declaration that could change what a shared module-scope binding holds is
/// reachable from every other declaration that reads it: that is how an edit to one
/// travels to the other without either naming the other.
///
/// Only the writers pull. Two declarations that merely read the same binding cannot
/// affect each other through it, which is what keeps sibling components apart in
/// code where every one of them calls the same helper.
fn apply_shared_state(
    drafts: &[DeclDraft],
    decl_by_name: &AHashMap<&str, DeclId>,
    users_of: &AHashMap<SymbolId, Vec<DeclId>>,
    mutators_of: &AHashMap<SymbolId, Vec<DeclId>>,
    ctx: &Ctx<'_>,
    decls: &mut [Decl],
) {
    let scoping = ctx.semantic.scoping();

    for (symbol_id, users) in users_of {
        if users.len() < 2 {
            continue;
        }
        let Some(mutators) = mutators_of.get(symbol_id) else {
            continue;
        };

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
            for &mutator in mutators {
                if user != mutator {
                    push_unique(&mut decls[user as usize].refs, mutator);
                }
            }
        }
    }
}

/// How a declaration touches a binding.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Use {
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
fn classify(nodes: &AstNodes<'_>, node_id: NodeId) -> Use {
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
/// CommonJS yields the whole export object, so the dependency is on all of it:
/// [`ImportTarget::Namespace`], the same target an `import * as ns` gets. A call in a
/// statement that declares nothing runs when the module is evaluated, so its target
/// joins the bare sources instead.
pub(crate) fn attach_requires(
    ctx: &Ctx<'_>,
    drafts: &[DeclDraft],
    requires: &[RequireCall],
    decls: &mut [Decl],
) -> Vec<SourceId> {
    let statement_decls = statement_decls(drafts);
    let mut init_sources = Vec::new();

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

        for &user in users {
            push_import(
                &mut decls[user as usize].imports,
                ImportRef {
                    source: call.source,
                    target: ImportTarget::Namespace,
                },
            );
        }
    }

    init_sources
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
