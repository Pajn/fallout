//! A deliberately small proof for local, hoisted helpers. Unlike the general
//! initialiser heuristic, every accepted expression must be known effect-free.
use ahash::AHashSet;
use oxc_ast::ast::*;
use oxc_semantic::SymbolId;

use super::parse::Ctx;

pub(super) struct LocalPure<'c, 'a> {
    ctx: &'c Ctx<'a>,
    proven: AHashSet<SymbolId>,
}

impl<'c, 'a> LocalPure<'c, 'a> {
    pub(super) fn infer(ctx: &'c Ctx<'a>, program: &Program<'a>) -> Self {
        let mut result = Self {
            ctx,
            proven: AHashSet::default(),
        };
        let candidates: Vec<_> = program
            .body
            .iter()
            .filter_map(|statement| match statement {
                Statement::FunctionDeclaration(function) => Some(function.as_ref()),
                Statement::ExportDeclaration(export) => match &export.declaration {
                    Declaration::FunctionDeclaration(function) => Some(function.as_ref()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let scoping = ctx.semantic.scoping();
        let candidates: Vec<_> = candidates
            .into_iter()
            .filter_map(|function| {
                let symbol = function.id.as_ref()?.symbol_id.get()?;
                // A var initialiser or a second function declaration can overwrite
                // the same hoisted binding without an assignment reference.
                if scoping.symbol_scope_id(symbol) != scoping.root_scope_id()
                    || !scoping.symbol_redeclarations(symbol).is_empty()
                    || scoping
                        .get_resolved_reference_ids(symbol)
                        .iter()
                        .any(|id| scoping.get_reference(*id).is_write())
                    || function.r#async
                    || function.generator
                    || function.params.rest.is_some()
                {
                    return None;
                }
                let mut params = AHashSet::default();
                for param in &function.params.items {
                    let BindingPattern::BindingIdentifier(id) = &param.pattern else {
                        return None;
                    };
                    if param.initializer.is_some() || !param.decorators.is_empty() {
                        return None;
                    }
                    params.insert(id.symbol_id.get()?);
                }
                let body = function.body.as_ref()?;
                let [Statement::ReturnStatement(returned)] = body.statements.as_slice() else {
                    return None;
                };
                Some((symbol, params, returned.argument.as_ref()))
            })
            .collect();
        // A least fixed point proves leaves first. Recursive cycles never acquire
        // a proof, even if their syntax otherwise fits the subset.
        loop {
            let previous = result.proven.len();
            for (symbol, params, returned) in &candidates {
                if !result.proven.contains(symbol)
                    && returned.is_none_or(|expr| result.expression(expr, params))
                {
                    result.proven.insert(*symbol);
                }
            }
            if result.proven.len() == previous {
                break;
            }
        }
        result
    }

    pub(super) fn call(&self, call: &CallExpression<'_>) -> bool {
        self.call_with_params(call, &AHashSet::default())
    }

    fn call_with_params(&self, call: &CallExpression<'_>, params: &AHashSet<SymbolId>) -> bool {
        let Expression::Identifier(id) = &call.callee else {
            return false;
        };
        self.symbol(id).is_some_and(|id| self.proven.contains(&id))
            && call.arguments.iter().all(|arg| {
                arg.as_expression()
                    .is_some_and(|expr| self.expression(expr, params))
            })
    }

    fn symbol(&self, id: &IdentifierReference<'_>) -> Option<SymbolId> {
        id.reference_id
            .get()
            .and_then(|id| self.ctx.semantic.scoping().get_reference(id).symbol_id())
    }

    fn expression(&self, expr: &Expression<'_>, params: &AHashSet<SymbolId>) -> bool {
        match expr {
            Expression::BooleanLiteral(_)
            | Expression::NullLiteral(_)
            | Expression::NumericLiteral(_)
            | Expression::BigIntLiteral(_)
            | Expression::StringLiteral(_) => true,
            Expression::Identifier(id) => self.symbol(id).is_some_and(|id| params.contains(&id)),
            Expression::ParenthesizedExpression(expr) => self.expression(&expr.expression, params),
            Expression::ArrayExpression(array) => array.elements.iter().all(|element| {
                matches!(element, ArrayExpressionElement::Elision(_))
                    || element
                        .as_expression()
                        .is_some_and(|expr| self.expression(expr, params))
            }),
            Expression::ObjectExpression(object) => object.properties.iter().all(|property| {
                let ObjectPropertyKind::ObjectProperty(property) = property else {
                    return false;
                };
                property.kind == PropertyKind::Init
                    && !property.method
                    && !property.computed
                    && self.expression(&property.value, params)
            }),
            Expression::ConditionalExpression(expr) => {
                self.expression(&expr.test, params)
                    && self.expression(&expr.consequent, params)
                    && self.expression(&expr.alternate, params)
            }
            Expression::LogicalExpression(expr) => {
                self.expression(&expr.left, params) && self.expression(&expr.right, params)
            }
            Expression::BinaryExpression(expr) => {
                matches!(expr.operator.as_str(), "===" | "!==")
                    && self.expression(&expr.left, params)
                    && self.expression(&expr.right, params)
            }
            Expression::UnaryExpression(expr) => {
                matches!(expr.operator.as_str(), "!" | "void" | "typeof")
                    && self.expression(&expr.argument, params)
            }
            Expression::CallExpression(call) => self.call_with_params(call, params),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::module::{ModuleAnalysis, Reading, parse::analyse_source};
    use std::path::Path;

    fn init(source: &str) -> Vec<String> {
        let (ModuleAnalysis::Fine(module), _) =
            analyse_source(Path::new("helpers.js"), source, &Reading::default()).unwrap()
        else {
            panic!("expected fine module: {source}")
        };
        module
            .init_decls
            .iter()
            .map(|&id| module.decls[id as usize].name.clone())
            .collect()
    }

    #[test]
    fn proves_simple_factories_and_transitive_calls_by_binding() {
        for source in [
            "function make(value) { return { value, list: [value, null] }; } export const result = make(1);",
            "function outer(x) { return inner(x); } function inner(x) { return x; } export const result = outer('ok');",
            "export function make(x) { return x ? { yes: !x } : { no: x === null }; } export const result = make(false);",
            "function make() { return; } export const result = make();",
        ] {
            assert!(init(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn unknown_body_effects_and_binding_changes_stay_in_initialisation() {
        for source in [
            "function make(x) { return x.value; } const result = make({ value: 1 });",
            "function make(x) { return x + 1; } const result = make(1);",
            "function make(x) { return `${x}`; } const result = make(1);",
            "function make(x) { return { ...x }; } const result = make({});",
            "function make(x) { return { [x]: 1 }; } const result = make('x');",
            "function make(x) { return x++; } const result = make(1);",
            "let state = 0; function make() { return state = 1; } const result = make();",
            "let state = 0; function make() { return state; } const result = make();",
            "function make(x = sideEffect()) { return x; } const result = make();",
            "function make({x}) { return x; } const result = make({x: 1});",
            "function make(...xs) { return xs; } const result = make(1);",
            "async function make() { return 1; } const result = make();",
            "function* make() { return 1; } const result = make();",
            "function make() { return unknown(); } const result = make();",
            "function make() { return make(); } const result = make();",
            "function make() { return other(); } function other() { return make(); } const result = make();",
            "function pure() { return 1; } function make(pure) { return pure(); } const result = make();",
            "function make() { return 1; } make = unknown; const result = make();",
            "function make() { return 1; } function make() { return unknown(); } const result = make();",
            "function make() { return 1; } var make = unknown; const result = make();",
            "function make() { return 1; } var { make } = unknown; const result = make();",
            "export default function make() { return 1; } const result = make();",
            "const make = () => 1; const result = make();",
            "function make() { return 1; } const result = new make();",
            "function make() { return 1; } const result = make``;",
        ] {
            assert!(init(source).contains(&"result".to_string()), "{source}");
        }
    }

    #[test]
    fn arguments_require_their_own_proof_even_when_unused() {
        for argument in [
            "unknown()",
            "obj.value",
            "...items",
            "x++",
            "x = 1",
            "unknown",
            "{ get x() { return 1; } }",
            "{ ...obj }",
            "null.value",
            "1n + 1",
        ] {
            let source =
                format!("function make() {{ return 1; }} const result = make({argument});");
            assert!(init(&source).contains(&"result".to_string()), "{source}");
        }
    }
}
