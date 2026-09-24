//! A deliberately small proof for local helpers. Unlike the general initialiser
//! heuristic, every accepted expression must be known effect-free.
//!
//! A helper is a top-level function declaration, or a `const` bound to an arrow or
//! a function expression. Its body is a run of `const` locals, `if`s that return,
//! and a final return, built from the expressions below. A call is pure where the
//! callee is a proven helper and every argument is itself provable.
//!
//! Where a call is written matters as much as what it calls. A function
//! declaration can be called before the line that declares it; a `const` cannot,
//! and calling one early throws, which is an effect. So every proof carries the
//! position from which it holds — the end of the latest `const` it depends on,
//! helper or value — and a call written before that position is not proven.
use ahash::AHashMap;
use oxc_ast::ast::*;
use oxc_semantic::SymbolId;

use super::parse::Ctx;

/// A position in the file from which a proof holds. Zero for one that holds
/// everywhere.
type Ready = u32;

pub(super) struct LocalPure<'c, 'a> {
    ctx: &'c Ctx<'a>,
    /// Proven helpers, and where each becomes callable.
    proven: AHashMap<SymbolId, Ready>,
    /// Top-level bindings reading which runs nothing — a `const` holding a
    /// primitive, or any function, since reading one does not call it — and where
    /// each becomes readable.
    values: AHashMap<SymbolId, Ready>,
}

/// The locals of the body being proven, each bound once and readable anywhere
/// after the statement that binds it.
type Locals = AHashMap<SymbolId, ()>;

struct Candidate<'s, 'a> {
    symbol: SymbolId,
    /// Where the binding itself becomes callable.
    ready: Ready,
    params: Vec<SymbolId>,
    body: Body<'s, 'a>,
}

enum Body<'s, 'a> {
    Statements(&'s [Statement<'a>]),
    Expression(&'s Expression<'a>),
}

impl<'c, 'a> LocalPure<'c, 'a> {
    pub(super) fn infer(ctx: &'c Ctx<'a>, program: &Program<'a>) -> Self {
        let mut result = Self {
            ctx,
            proven: AHashMap::default(),
            values: AHashMap::default(),
        };
        let scoping = ctx.semantic.scoping();
        let root = scoping.root_scope_id();
        // A var initialiser or a second function declaration can overwrite the
        // same hoisted binding without an assignment reference.
        let stable = |symbol: SymbolId| {
            scoping.symbol_scope_id(symbol) == root
                && scoping.symbol_redeclarations(symbol).is_empty()
                && !scoping
                    .get_resolved_reference_ids(symbol)
                    .iter()
                    .any(|id| scoping.get_reference(*id).is_write())
        };

        let mut candidates = Vec::new();
        for statement in &program.body {
            let declaration = match statement {
                Statement::ExportDeclaration(export) => &export.declaration,
                statement => match statement.as_declaration() {
                    Some(declaration) => declaration,
                    None => continue,
                },
            };
            match declaration {
                Declaration::FunctionDeclaration(function) => {
                    let Some(symbol) = function.id.as_ref().and_then(|id| id.symbol_id.get())
                    else {
                        continue;
                    };
                    if !stable(symbol) {
                        continue;
                    }
                    result.values.insert(symbol, 0);
                    if let Some(candidate) = function_candidate(symbol, 0, function) {
                        candidates.push(candidate);
                    }
                }
                Declaration::VariableDeclaration(variable)
                    if variable.kind == VariableDeclarationKind::Const =>
                {
                    for declarator in &variable.declarations {
                        let BindingPattern::BindingIdentifier(id) = &declarator.id else {
                            continue;
                        };
                        let (Some(symbol), Some(init)) = (id.symbol_id.get(), &declarator.init)
                        else {
                            continue;
                        };
                        if !stable(symbol) {
                            continue;
                        }
                        let ready = declarator.span.end;
                        match init.get_inner_expression() {
                            Expression::ArrowFunctionExpression(arrow) => {
                                result.values.insert(symbol, ready);
                                if let Some(candidate) = arrow_candidate(symbol, ready, arrow) {
                                    candidates.push(candidate);
                                }
                            }
                            Expression::FunctionExpression(function) => {
                                result.values.insert(symbol, ready);
                                if let Some(candidate) = function_candidate(symbol, ready, function)
                                {
                                    candidates.push(candidate);
                                }
                            }
                            init if is_primitive(init) => {
                                result.values.insert(symbol, ready);
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        // A least fixed point proves leaves first. Recursive cycles never acquire
        // a proof, even if their syntax otherwise fits the subset.
        loop {
            let previous = result.proven.len();
            for candidate in &candidates {
                if result.proven.contains_key(&candidate.symbol) {
                    continue;
                }
                let mut locals: Locals = candidate.params.iter().map(|&p| (p, ())).collect();
                let proof = match &candidate.body {
                    Body::Expression(expr) => result.expression(expr, &locals),
                    Body::Statements(statements) => result.statements(statements, &mut locals),
                };
                if let Some(ready) = proof {
                    result
                        .proven
                        .insert(candidate.symbol, ready.max(candidate.ready));
                }
            }
            if result.proven.len() == previous {
                break;
            }
        }
        result
    }

    /// Whether a top-level call is proven to run nothing where it is written.
    pub(super) fn call(&self, call: &CallExpression<'_>) -> bool {
        self.call_ready(call, &Locals::default())
            .is_some_and(|ready| ready <= call.span.start)
    }

    /// Whether a call is `Object.freeze` of a literal made on the spot, which
    /// nobody but the literal's holder can observe. What the literal holds is the
    /// caller's to judge, as it is for any other call.
    pub(super) fn freezes(&self, call: &CallExpression<'_>) -> bool {
        frozen_literal(self.ctx, call).is_some()
    }

    fn call_ready(&self, call: &CallExpression<'_>, locals: &Locals) -> Option<Ready> {
        let mut ready = if let Some(literal) = frozen_literal(self.ctx, call) {
            // Freezing a literal made on the spot is invisible to anyone but its
            // holder, and the literal's contents are proven as any other.
            return self.expression(literal, locals);
        } else {
            let Expression::Identifier(id) = &call.callee else {
                return None;
            };
            *self.proven.get(&self.symbol(id)?)?
        };
        for argument in &call.arguments {
            ready = ready.max(self.expression(argument.as_expression()?, locals)?);
        }
        Some(ready)
    }

    fn symbol(&self, id: &IdentifierReference<'_>) -> Option<SymbolId> {
        id.reference_id
            .get()
            .and_then(|id| self.ctx.semantic.scoping().get_reference(id).symbol_id())
    }

    /// A body proven statement by statement: each `const` local is readable only
    /// once its own statement has been proven, so a read before it fails.
    fn statements(&self, statements: &[Statement<'_>], locals: &mut Locals) -> Option<Ready> {
        let mut ready = 0;
        for statement in statements {
            ready = ready.max(self.statement(statement, locals)?);
        }
        Some(ready)
    }

    fn statement(&self, statement: &Statement<'_>, locals: &mut Locals) -> Option<Ready> {
        match statement {
            Statement::ReturnStatement(returned) => match &returned.argument {
                Some(argument) => self.expression(argument, locals),
                None => Some(0),
            },
            Statement::VariableDeclaration(variable)
                if variable.kind == VariableDeclarationKind::Const =>
            {
                let mut ready = 0;
                for declarator in &variable.declarations {
                    let BindingPattern::BindingIdentifier(id) = &declarator.id else {
                        return None;
                    };
                    ready = ready.max(self.expression(declarator.init.as_ref()?, locals)?);
                    locals.insert(id.symbol_id.get()?, ());
                }
                Some(ready)
            }
            Statement::IfStatement(branch) => {
                let mut ready = self.expression(&branch.test, locals)?;
                ready = ready.max(self.statement(&branch.consequent, locals)?);
                if let Some(alternate) = &branch.alternate {
                    ready = ready.max(self.statement(alternate, locals)?);
                }
                Some(ready)
            }
            Statement::BlockStatement(block) => self.statements(&block.body, locals),
            Statement::EmptyStatement(_) => Some(0),
            _ => None,
        }
    }

    fn expression(&self, expr: &Expression<'_>, locals: &Locals) -> Option<Ready> {
        let all = |exprs: &[&Expression<'_>]| {
            exprs.iter().try_fold(0, |ready: Ready, expr| {
                Some(ready.max(self.expression(expr, locals)?))
            })
        };
        match expr {
            expr if is_primitive(expr) => Some(0),
            Expression::TemplateLiteral(template) if template.expressions.is_empty() => Some(0),
            Expression::Identifier(id) => {
                let symbol = self.symbol(id)?;
                if locals.contains_key(&symbol) {
                    Some(0)
                } else {
                    self.values.get(&symbol).copied()
                }
            }
            Expression::ParenthesizedExpression(expr) => self.expression(&expr.expression, locals),
            Expression::ArrayExpression(array) => {
                let mut ready = 0;
                for element in &array.elements {
                    if matches!(element, ArrayExpressionElement::Elision(_)) {
                        continue;
                    }
                    ready = ready.max(self.expression(element.as_expression()?, locals)?);
                }
                Some(ready)
            }
            Expression::ObjectExpression(object) => {
                let mut ready = 0;
                for property in &object.properties {
                    let ObjectPropertyKind::ObjectProperty(property) = property else {
                        return None;
                    };
                    if property.kind != PropertyKind::Init || property.method || property.computed {
                        return None;
                    }
                    ready = ready.max(self.expression(&property.value, locals)?);
                }
                Some(ready)
            }
            Expression::ConditionalExpression(expr) => {
                all(&[&expr.test, &expr.consequent, &expr.alternate])
            }
            Expression::LogicalExpression(expr) => all(&[&expr.left, &expr.right]),
            Expression::BinaryExpression(expr)
                if matches!(expr.operator.as_str(), "===" | "!==") =>
            {
                all(&[&expr.left, &expr.right])
            }
            Expression::UnaryExpression(expr)
                if matches!(expr.operator.as_str(), "!" | "void" | "typeof") =>
            {
                self.expression(&expr.argument, locals)
            }
            Expression::CallExpression(call) => self.call_ready(call, locals),
            _ => None,
        }
    }
}

fn function_candidate<'s, 'a>(
    symbol: SymbolId,
    ready: Ready,
    function: &'s Function<'a>,
) -> Option<Candidate<'s, 'a>> {
    if function.r#async || function.generator {
        return None;
    }
    Some(Candidate {
        symbol,
        ready,
        params: simple_params(&function.params)?,
        body: Body::Statements(&function.body.as_ref()?.statements),
    })
}

fn arrow_candidate<'s, 'a>(
    symbol: SymbolId,
    ready: Ready,
    arrow: &'s ArrowFunctionExpression<'a>,
) -> Option<Candidate<'s, 'a>> {
    if arrow.r#async {
        return None;
    }
    let body = match &arrow.body {
        ArrowFunctionBody::FunctionBody(body) => Body::Statements(&body.statements),
        body => Body::Expression(body.as_expression()?),
    };
    Some(Candidate {
        symbol,
        ready,
        params: simple_params(&arrow.params)?,
        body,
    })
}

/// Plain named parameters, or `None`: a default runs, a pattern reads properties
/// that may be getters, and a rest parameter builds an array from `arguments`.
fn simple_params(params: &FormalParameters<'_>) -> Option<Vec<SymbolId>> {
    if params.rest.is_some() {
        return None;
    }
    params
        .items
        .iter()
        .map(|param| {
            let BindingPattern::BindingIdentifier(id) = &param.pattern else {
                return None;
            };
            if param.initializer.is_some() || !param.decorators.is_empty() {
                return None;
            }
            id.symbol_id.get()
        })
        .collect()
}

/// A literal with no interior, including a negated number.
fn is_primitive(expr: &Expression<'_>) -> bool {
    match expr {
        Expression::BooleanLiteral(_)
        | Expression::NullLiteral(_)
        | Expression::NumericLiteral(_)
        | Expression::BigIntLiteral(_)
        | Expression::StringLiteral(_) => true,
        Expression::UnaryExpression(unary) => {
            matches!(unary.operator.as_str(), "-" | "+")
                && matches!(
                    unary.argument,
                    Expression::NumericLiteral(_) | Expression::BigIntLiteral(_)
                )
                // `+1n` throws.
                && !(unary.operator.as_str() == "+"
                    && matches!(unary.argument, Expression::BigIntLiteral(_)))
        }
        _ => false,
    }
}

/// The literal in `Object.freeze({ ... })` or `Object.freeze([ ... ])`, where
/// `Object` is the global rather than a binding of the file.
fn frozen_literal<'e, 'a>(
    ctx: &Ctx<'_>,
    call: &'e CallExpression<'a>,
) -> Option<&'e Expression<'a>> {
    let Expression::StaticMemberExpression(callee) = &call.callee else {
        return None;
    };
    let Expression::Identifier(object) = &callee.object else {
        return None;
    };
    let global = object.name == "Object"
        && callee.property.name == "freeze"
        && object.reference_id.get().is_some_and(|id| {
            ctx.semantic
                .scoping()
                .get_reference(id)
                .symbol_id()
                .is_none()
        });
    let [argument] = call.arguments.as_slice() else {
        return None;
    };
    let argument = argument.as_expression()?;
    (global
        && matches!(
            argument.get_inner_expression(),
            Expression::ObjectExpression(_) | Expression::ArrayExpression(_)
        ))
    .then_some(argument)
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
            "function make() { return 1; } const result = new make();",
            "function make() { return 1; } const result = make``;",
        ] {
            assert!(init(source).contains(&"result".to_string()), "{source}");
        }
    }

    #[test]
    fn const_helpers_longer_bodies_constants_and_frozen_literals_are_proven() {
        for source in [
            "const make = (value) => ({ value }); export const result = make(1);",
            "const make = (value) => { return { value }; }; export const result = make(1);",
            "const make = function (value) { return [value]; }; export const result = make(1);",
            "export const make = (x) => x; export const result = make('ok');",
            "const LABEL = 'item'; function make(x) { return { x }; } export const result = make(LABEL);",
            "const LABEL = 'item'; const MIN = -1; function make() { return { LABEL, MIN }; } export const result = make();",
            "const make = (x) => ({ x }); const wrap = (x) => make(x); export const result = wrap(`plain`);",
            "function make(x) { const y = { x }; const z = [y]; return z; } export const result = make(1);",
            "function make(x) { if (x === null) return null; if (x) { return { x }; } else return [x]; } export const result = make(1);",
            "function make(x) { const y = [x]; } export const result = make(1);",
            "export const Colors = Object.freeze({ red: '#f00', blue: '#00f' });",
            "export const Sizes = Object.freeze(['s', 'm', 'l']);",
            "function make(x) { return Object.freeze({ x }); } export const result = make(1);",
            "function describe() { return describe; } const make = () => describe; export const result = make();",
        ] {
            assert!(init(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn a_call_before_what_it_depends_on_is_ready_stays_in_initialisation() {
        for source in [
            // A `const` is not initialised until its declaration runs, and calling
            // or reading one before then throws.
            "const result = make(); const make = () => 1;",
            "const result = make(); const make = function () { return 1; };",
            "const result = make(LABEL); function make(x) { return x; } const LABEL = 'a';",
            "function make() { return LABEL; } const result = make(); const LABEL = 'a';",
            "const make = () => inner(); const result = make(); const inner = () => 1;",
            "function make() { return inner(); } const result = make(); const inner = () => 1;",
        ] {
            assert!(init(source).contains(&"result".to_string()), "{source}");
        }
    }

    #[test]
    fn statements_and_values_outside_the_subset_stay_in_initialisation() {
        for source in [
            // A local read before its own declaration throws.
            "function make() { const a = b; const b = 1; return a; } const result = make();",
            "function make() { let a = 1; return a; } const result = make();",
            "function make(x) { for (const y of x) {} return 1; } const result = make([]);",
            "function make(x) { x; return 1; } const result = make(1);",
            "function make() { throw 1; } const result = make();",
            "const make = async () => 1; const result = make();",
            "const make = (...xs) => xs; const result = make(1);",
            "const make = ({ x }) => x; const result = make({ x: 1 });",
            "const make = (x = 1) => x; const result = make();",
            "const make = () => this; const result = make();",
            "const make = () => arguments; const result = make();",
            "const make = () => `${1}`; const result = make();",
            "const make = () => +1n; const result = make();",
            "let make = () => 1; const result = make();",
            "var make = () => 1; const result = make();",
            "const make = () => 1, other = make(); const result = make.call();",
            // A binding that only looks like the global.
            "const Object = { freeze: (x) => x }; const result = Object.freeze({});",
            "const result = Object.freeze(make());",
            "const result = Object.freeze({}, extra);",
            "const result = Object.seal({});",
            "function make(x) { return Object.freeze({ x: other() }); } const result = make(1);",
        ] {
            assert!(init(source).contains(&"result".to_string()), "{source}");
        }
    }

    #[test]
    fn freezing_says_nothing_about_what_the_literal_holds() {
        let source = "export const Colors = Object.freeze({ red: register('red') });";
        assert!(init(source).contains(&"Colors".to_string()), "{source}");
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
