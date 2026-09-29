//! What happens to a value where it is used: whether it stays in place, or leaves
//! the expression it is read in for code that could change it.
//!
//! The shared-state rule asks this of every use of a mutable module-scope binding,
//! and of what a Zustand store or a collection the file makes hands out. The walk
//! out through the wrappers that leave a value as it was is the same for all of
//! them, so this module holds it.

use oxc_ast::AstKind;
use oxc_ast::ast::JSXElementName;
use oxc_semantic::{AstNodes, NodeId};
use oxc_span::{GetSpan, Span as OxcSpan};

/// Whether a value in the JSX expression container at `container` is only
/// rendered where it stands, rather than handed to code that could change it.
///
/// A component is handed its children as a prop, free to call them or change what
/// they hold, as it is handed every other prop. An element of the platform's own,
/// such as `<li>`, or a fragment, only renders its children. React takes `key` for
/// itself, as a string, and hands it to no component. Any other prop is handed on,
/// even by an element of the platform's own, which may call it as a handler.
pub(crate) fn rendered_in_place(nodes: &AstNodes<'_>, container: NodeId) -> bool {
    match nodes.parent_kind(container) {
        AstKind::JSXElement(element) => {
            matches!(element.opening_element.name, JSXElementName::Identifier(_))
        }
        AstKind::JSXFragment(_) => true,
        AstKind::JSXAttribute(attribute) => attribute.is_key(),
        _ => false,
    }
}

/// Walks out through parentheses and type-only wrappers, which leave the value as
/// it was, returning the outermost node standing for it and its span.
pub(crate) fn through_wrappers(nodes: &AstNodes<'_>, node_id: NodeId) -> (NodeId, OxcSpan) {
    let mut current = node_id;
    let mut span = nodes.get_node(node_id).kind().span();
    loop {
        match nodes.parent_kind(current) {
            AstKind::ParenthesizedExpression(_)
            | AstKind::TSNonNullExpression(_)
            | AstKind::TSAsExpression(_)
            | AstKind::TSSatisfiesExpression(_)
            | AstKind::TSTypeAssertion(_) => {
                current = nodes.parent_id(current);
                span = nodes.get_node(current).kind().span();
            }
            _ => return (current, span),
        }
    }
}
