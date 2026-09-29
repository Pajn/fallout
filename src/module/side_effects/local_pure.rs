//! A deliberately small proof for local helpers. Unlike the general initialiser
//! heuristic, every accepted expression must be known effect-free.
//!
//! A helper is a top-level function declaration, or a `const` bound to an arrow or
//! a function expression. Its body is a run of `const` locals, `if`s that return,
//! statements such as `console.log(value);` whose expression is itself provable,
//! and a final return, built from the expressions below. An import may be read
//! directly, as the module body may, but not read through. A call is pure where the
//! callee is a proven helper, or one the project's list or a `/* @__PURE__ */`
//! annotation clears, and every argument is itself provable. The list and the
//! annotation speak for the call alone, so its arguments are proven here as any
//! other's. Creating a function — an arrow, a function expression or an object
//! literal's method — runs nothing, since its body runs only when it is called, and
//! a call to one is proven by the same rules as any other call.
//!
//! A proven call must also not throw. A throw is not a side effect, but every call
//! this is asked about is written in the module body, and runs whatever helpers it
//! reaches there: a throw stops the module loading, and every importer with it. So
//! a helper's body is held to that too.
//!
//! Where a call is written therefore matters as much as what it calls. A function
//! declaration can be called before the line that declares it; a `const` cannot,
//! and calling or reading one early throws. So every proof carries the position from
//! which it holds — the end of the latest `const` it depends on, helper or value —
//! and a call written before that position is not proven.
//!
//! The same proof answers for a function a known factory calls while creating its
//! value, such as a Zustand store's creator: its body is held to the rules for a
//! helper's, with its parameters standing for values nobody here knows anything
//! about, and it runs where the factory is called.
use ahash::AHashMap;
use oxc_ast::ast::*;
use oxc_semantic::{IsGlobalReference, SymbolId};

use super::globals;
use super::imports::Imports;
use crate::module::parse::{Ctx, frozen};

/// A position in the file from which a proof holds. Zero for one that holds
/// everywhere.
type Ready = u32;

pub(super) struct LocalPure<'c, 'a> {
    ctx: &'c Ctx<'a>,
    /// Where each import of the file comes from, and what the project's list says
    /// of it.
    imports: Imports<'c>,
    /// Proven helpers, and where each becomes callable.
    proven: AHashMap<SymbolId, Ready>,
    /// Top-level bindings reading which runs nothing — a `const` holding a
    /// primitive, or any function, since reading one does not call it — and where
    /// each becomes readable.
    values: AHashMap<SymbolId, Ready>,
    /// Which of those values are primitives a conversion can be applied to, each
    /// with whether it is a BigInt, which a conversion to a number throws on.
    primitives: AHashMap<SymbolId, bool>,
}

/// How much of a call or a construction a proof covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Proof {
    /// Everything it runs, its arguments included.
    Whole,
    /// The call or construction itself. What its arguments run when they are
    /// evaluated is still to be judged, but their values are only held, never
    /// read.
    Invocation,
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
    pub(super) fn infer(ctx: &'c Ctx<'a>, program: &Program<'a>, imports: Imports<'c>) -> Self {
        let mut result = Self {
            ctx,
            imports,
            proven: AHashMap::default(),
            values: AHashMap::default(),
            primitives: AHashMap::default(),
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
                                result.primitives.insert(symbol, is_bigint(init));
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
                if let Some(ready) = result.body(&candidate.params, &candidate.body) {
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

    pub(super) fn imports(&self) -> &Imports<'c> {
        &self.imports
    }

    /// Whether calling `function`, a function written out in place, once at `at`
    /// with arguments nobody here knows anything about, is proven to run nothing
    /// and not to throw.
    ///
    /// A factory that calls what it is given while creating its value asks this:
    /// Zustand calls a store's creator as it makes the store. The parameters are
    /// held as a helper's are, so the body may hand them on or return them, but
    /// calling one, or reading through it, is not proven. The body runs at `at`, so
    /// what it reads must be ready there. Middleware around the function is looked
    /// through before this is asked, in [`super::wrappers`].
    pub(super) fn invoked(&self, function: &Expression<'_>, at: u32) -> bool {
        let parts = match function.get_inner_expression() {
            Expression::ArrowFunctionExpression(arrow) => arrow_parts(arrow),
            Expression::FunctionExpression(function) => function_parts(function),
            _ => None,
        };
        parts
            .and_then(|(params, body)| self.body(&params, &body))
            .is_some_and(|ready| ready <= at)
    }

    /// From where running `body`, with `params` bound to values nobody here knows
    /// anything about, runs nothing and does not throw.
    fn body(&self, params: &[SymbolId], body: &Body<'_, '_>) -> Option<Ready> {
        let mut locals: Locals = params.iter().map(|&p| (p, ())).collect();
        match body {
            Body::Expression(expr) => self.expression(expr, &locals),
            Body::Statements(statements) => self.statements(statements, &mut locals),
        }
    }

    /// How much of a top-level call is proven to run nothing where it is written.
    ///
    /// A call to a proven helper, to a global without side effects, or to a callee
    /// the list or an annotation clears, is proven whole: its arguments are handed to
    /// code that may convert or return them, so they are held to this proof too.
    /// `Object.freeze` of a literal made on the spot is proven only as a call, since
    /// freezing is invisible to anyone but the literal's holder and never reads what
    /// the literal holds.
    pub(super) fn call(&self, call: &CallExpression<'_>) -> Option<Proof> {
        if self
            .call_ready(call, &Locals::default())
            .is_some_and(|ready| ready <= call.span.start)
        {
            return Some(Proof::Whole);
        }
        frozen_literal(self.ctx, call).map(|_| Proof::Invocation)
    }

    /// How much of a top-level `new` is proven to run nothing where it is written.
    ///
    /// A collection filled from an array literal is proven only as a
    /// construction where the literal's elements are not themselves proven:
    /// iterating an array literal reads its elements and runs nothing, as freezing
    /// a literal does.
    pub(super) fn construct(&self, new: &NewExpression<'_>) -> Option<Proof> {
        let proven = |contents| {
            self.construct_ready(new, &Locals::default(), contents)
                .is_some_and(|ready| ready <= new.span.start)
        };
        if proven(true) {
            Some(Proof::Whole)
        } else if proven(false) {
            Some(Proof::Invocation)
        } else {
            None
        }
    }

    fn call_ready(&self, call: &CallExpression<'_>, locals: &Locals) -> Option<Ready> {
        if let Some((conversion, _)) = self.global_call(call) {
            return self.converted(&call.arguments, conversion, locals);
        }
        let mut ready = if let Some(literal) = frozen_literal(self.ctx, call) {
            // Freezing a literal made on the spot is invisible to anyone but its
            // holder, and the literal's contents are proven as any other.
            return self.expression(literal, locals);
        } else {
            self.callee_ready(call, locals)?
        };
        for argument in &call.arguments {
            ready = ready.max(self.expression(argument.as_expression()?, locals)?);
        }
        Some(ready)
    }

    /// From where calling `call`'s callee runs nothing, what its arguments run
    /// aside.
    ///
    /// A proven helper does from where it becomes callable. So does a callee the
    /// project's list names, reached from the import the entry names, which is bound
    /// before anything in the module runs, and any callee of a call annotated
    /// `/* @__PURE__ */`. Those two are claims rather than proofs, and are trusted
    /// not to throw either, as bundlers trust them when they drop such a call. The
    /// annotation speaks for the call and not for reading the callee, so a callee
    /// read before its declaration has run is not proven.
    fn callee_ready(&self, call: &CallExpression<'_>, locals: &Locals) -> Option<Ready> {
        if let Expression::Identifier(id) = &call.callee
            && let Some(&ready) = self.symbol(id).and_then(|symbol| self.proven.get(&symbol))
        {
            return Some(ready);
        }
        if self.imports.listed(&call.callee) {
            return Some(0);
        }
        if call.pure {
            return self.root_ready(&call.callee, locals);
        }
        None
    }

    /// From where reading the binding at the root of an annotated callee `a.b.c`
    /// does not throw: a parameter or a local, an import, or a top-level function
    /// or `const` primitive once its declaration has run. A global is trusted to
    /// exist, as the annotation is trusted for the call. Reading the members after
    /// the root is assumed to run nothing, as reading a property is everywhere else.
    fn root_ready(&self, expr: &Expression<'_>, locals: &Locals) -> Option<Ready> {
        let root = match expr {
            Expression::Identifier(id) => id,
            Expression::StaticMemberExpression(member) => {
                return self.root_ready(&member.object, locals);
            }
            _ => return None,
        };
        let Some(symbol) = self.symbol(root) else {
            return Some(0);
        };
        if locals.contains_key(&symbol) || self.imports.is_import(root) {
            Some(0)
        } else {
            self.values.get(&symbol).copied()
        }
    }

    /// The name a callee or an object goes by, where it is a global rather than a
    /// binding of the file.
    fn global<'e>(&self, expr: &'e Expression<'_>) -> Option<&'e str> {
        match expr {
            Expression::Identifier(id) if id.is_global_reference(self.ctx.semantic.scoping()) => {
                Some(id.name.as_str())
            }
            _ => None,
        }
    }

    /// A call to a global function or namespace method that has no side effects.
    fn global_call(
        &self,
        call: &CallExpression<'_>,
    ) -> Option<(globals::Conversion, globals::Returns)> {
        match &call.callee {
            Expression::StaticMemberExpression(member) => {
                globals::method(self.global(&member.object)?, member.property.name.as_str())
            }
            callee => globals::function(self.global(callee)?),
        }
    }

    /// Arguments proven safe to hand to a function that converts them as given.
    fn converted(
        &self,
        arguments: &[Argument<'_>],
        conversion: globals::Conversion,
        locals: &Locals,
    ) -> Option<Ready> {
        // A first argument the console may read as a format string, followed by
        // more, has to be one written out that holds no `%`: `%d`, `%j` and the rest
        // convert what follows, and throw on a BigInt or a symbol.
        if conversion == globals::Conversion::Shown
            && arguments.len() > 1
            && !arguments
                .first()
                .and_then(Argument::as_expression)
                .is_some_and(no_format)
        {
            return None;
        }
        let mut ready = 0;
        for argument in arguments {
            let argument = argument.as_expression()?;
            ready = ready.max(match conversion {
                globals::Conversion::None | globals::Conversion::Shown => {
                    self.expression(argument, locals)?
                }
                // `toString` and `valueOf` are taken to have no side effects, so what
                // a conversion can do beyond reading is throw. A value that converts
                // without is a primitive, a BigInt only to a string, or an object or
                // array literal that brings no conversion of its own. A parameter
                // could be a BigInt or a symbol, so it is not one.
                globals::Conversion::ToString => self.convertible(argument, locals, false)?,
                globals::Conversion::ToNumber => self.convertible(argument, locals, true)?,
            });
        }
        Some(ready)
    }

    fn construct_ready(
        &self,
        new: &NewExpression<'_>,
        locals: &Locals,
        contents: bool,
    ) -> Option<Ready> {
        use globals::Constructor;

        let constructor = globals::constructor(self.global(&new.callee)?)?;
        let entries = match constructor {
            Constructor::Converting(conversion) => {
                return self.converted(&new.arguments, conversion, locals);
            }
            Constructor::Set | Constructor::Map | Constructor::Empty => {
                match new.arguments.as_slice() {
                    [] => return Some(0),
                    [argument] => argument.as_expression()?,
                    _ => return None,
                }
            }
        };
        if matches!(entries, Expression::NullLiteral(_))
            || self.global(entries) == Some("undefined")
        {
            return Some(0);
        }
        let Expression::ArrayExpression(array) = entries else {
            return None;
        };
        let mut ready = 0;
        for element in &array.elements {
            let element = element.as_expression()?;
            match constructor {
                // A primitive key throws, and nothing here can tell one apart.
                Constructor::Empty => return None,
                // Each entry must be a pair written out, or it is read by iterating.
                Constructor::Map if !matches!(element, Expression::ArrayExpression(_)) => {
                    return None;
                }
                _ => {}
            }
            if contents {
                ready = ready.max(self.expression(element, locals)?);
            }
        }
        Some(ready)
    }

    /// A value that converts to a string, or a number, without throwing.
    fn convertible(&self, expr: &Expression<'_>, locals: &Locals, number: bool) -> Option<Ready> {
        match expr.get_inner_expression() {
            // Converting an array joins its elements, each converted to a string in
            // turn, so each is held to this as the array is.
            Expression::ArrayExpression(array) => {
                let mut ready = 0;
                for element in &array.elements {
                    if matches!(element, ArrayExpressionElement::Elision(_)) {
                        continue;
                    }
                    ready = ready.max(self.convertible(element.as_expression()?, locals, false)?);
                }
                Some(ready)
            }
            // Converting an object calls its `toString` or `valueOf`, so one that
            // names either, or sets the prototype they are found on, could run
            // anything or have neither to call. Any other it finds on
            // `Object.prototype`.
            Expression::ObjectExpression(object) => {
                let names_conversion = object.properties.iter().any(|property| {
                    let ObjectPropertyKind::ObjectProperty(property) = property else {
                        return false;
                    };
                    property.key.static_name().is_some_and(|name| {
                        matches!(name.as_ref(), "toString" | "valueOf" | "__proto__")
                    })
                });
                if names_conversion {
                    return None;
                }
                self.expression(expr, locals)
            }
            _ => self.primitive(expr, locals, number),
        }
    }

    /// A value proven to be a primitive a conversion can be applied to, running
    /// nothing: a string, a number, a boolean, `null` or `undefined`, and a BigInt
    /// unless the conversion is to a number.
    fn primitive(&self, expr: &Expression<'_>, locals: &Locals, number: bool) -> Option<Ready> {
        match expr {
            expr if is_primitive(expr) => (!number || !is_bigint(expr)).then_some(0),
            Expression::TemplateLiteral(template) if template.expressions.is_empty() => Some(0),
            Expression::Identifier(id) => match self.symbol(id) {
                Some(symbol) => {
                    let bigint = *self.primitives.get(&symbol)?;
                    (!number || !bigint).then(|| self.values.get(&symbol).copied())?
                }
                None => self
                    .global(expr)
                    .is_some_and(globals::primitive_global)
                    .then_some(0),
            },
            Expression::ParenthesizedExpression(inner) => {
                self.primitive(&inner.expression, locals, number)
            }
            // A boolean, a type name or `undefined`, whatever the operand was.
            Expression::UnaryExpression(unary)
                if matches!(unary.operator.as_str(), "!" | "void" | "typeof") =>
            {
                self.expression(&unary.argument, locals)
            }
            Expression::BinaryExpression(binary)
                if matches!(binary.operator.as_str(), "===" | "!==") =>
            {
                Some(
                    self.expression(&binary.left, locals)?
                        .max(self.expression(&binary.right, locals)?),
                )
            }
            Expression::ConditionalExpression(conditional) => Some(
                self.expression(&conditional.test, locals)?
                    .max(self.primitive(&conditional.consequent, locals, number)?)
                    .max(self.primitive(&conditional.alternate, locals, number)?),
            ),
            // Either operand can be the result.
            Expression::LogicalExpression(logical) => Some(
                self.primitive(&logical.left, locals, number)?
                    .max(self.primitive(&logical.right, locals, number)?),
            ),
            Expression::StaticMemberExpression(member) => self
                .global(&member.object)
                .is_some_and(|object| globals::constant(object, member.property.name.as_str()))
                .then_some(0),
            Expression::CallExpression(call) => match self.global_call(call)? {
                (conversion, globals::Returns::Primitive) => {
                    self.converted(&call.arguments, conversion, locals)
                }
                (_, globals::Returns::Other) => None,
            },
            _ => None,
        }
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
            // An expression run for nothing but its effects, where it has none that
            // anything reads back: `console.log(value);`.
            Statement::ExpressionStatement(statement) => {
                self.expression(&statement.expression, locals)
            }
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
                let Some(symbol) = self.symbol(id) else {
                    return self.primitive(expr, locals, false);
                };
                // An import is bound before anything in the module runs, and reading
                // one directly is taken to run nothing, as it is in the module body.
                // Its value is unknown, so it is not a primitive to convert, and a
                // member read through it is not proven.
                if locals.contains_key(&symbol) || self.imports.is_import(id) {
                    Some(0)
                } else {
                    self.values.get(&symbol).copied()
                }
            }
            Expression::StaticMemberExpression(_) => self.primitive(expr, locals, false),
            Expression::NewExpression(new) => self.construct_ready(new, locals, true),
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
                    // A computed key runs where the object is created. A method, a
                    // getter or a setter is a function created with it, whose value
                    // is proven as any other function's is.
                    if property.computed {
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
            // Creating a function runs nothing, async and generator ones included:
            // its defaults and its body wait for a call, and so does every read in
            // it, so what it reads need not be ready yet. A call to it is judged
            // where it is written, as any call is, and proving one here takes a
            // helper, the list or an annotation. A class expression is not one of
            // these, since defining it runs its heritage and static members.
            Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => Some(0),
            _ => None,
        }
    }
}

fn function_candidate<'s, 'a>(
    symbol: SymbolId,
    ready: Ready,
    function: &'s Function<'a>,
) -> Option<Candidate<'s, 'a>> {
    let (params, body) = function_parts(function)?;
    Some(Candidate {
        symbol,
        ready,
        params,
        body,
    })
}

fn arrow_candidate<'s, 'a>(
    symbol: SymbolId,
    ready: Ready,
    arrow: &'s ArrowFunctionExpression<'a>,
) -> Option<Candidate<'s, 'a>> {
    let (params, body) = arrow_parts(arrow)?;
    Some(Candidate {
        symbol,
        ready,
        params,
        body,
    })
}

/// The parameters and body of a function the proof can read. An async function
/// runs its body up to the first `await` and a generator returns an iterator, so
/// neither is one.
fn function_parts<'s, 'a>(function: &'s Function<'a>) -> Option<(Vec<SymbolId>, Body<'s, 'a>)> {
    if function.r#async || function.generator {
        return None;
    }
    Some((
        simple_params(&function.params)?,
        Body::Statements(&function.body.as_ref()?.statements),
    ))
}

fn arrow_parts<'s, 'a>(
    arrow: &'s ArrowFunctionExpression<'a>,
) -> Option<(Vec<SymbolId>, Body<'s, 'a>)> {
    if arrow.r#async {
        return None;
    }
    let body = match &arrow.body {
        ArrowFunctionBody::FunctionBody(body) => Body::Statements(&body.statements),
        body => Body::Expression(body.as_expression()?),
    };
    Some((simple_params(&arrow.params)?, body))
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

/// A literal the console cannot read as a format string: anything but a string, or
/// a string with no `%` in it.
fn no_format(expr: &Expression<'_>) -> bool {
    match expr {
        Expression::StringLiteral(literal) => !literal.value.contains('%'),
        Expression::TemplateLiteral(template) => {
            template.expressions.is_empty()
                && template
                    .quasis
                    .iter()
                    .all(|quasi| !quasi.value.raw.contains('%'))
        }
        expr => is_primitive(expr),
    }
}

/// A BigInt literal, which a conversion to a number throws on.
fn is_bigint(expr: &Expression<'_>) -> bool {
    match expr {
        Expression::BigIntLiteral(_) => true,
        Expression::UnaryExpression(unary) => {
            matches!(unary.argument, Expression::BigIntLiteral(_))
        }
        _ => false,
    }
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
    frozen(call, Some(ctx.semantic.scoping())).filter(|argument| {
        matches!(
            argument.get_inner_expression(),
            Expression::ObjectExpression(_) | Expression::ArrayExpression(_)
        )
    })
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
    fn global_functions_without_side_effects_are_proven() {
        for source in [
            "export const cache = new Map();",
            "export const seen = new Set(['a', 'b']);",
            "export const table = new Map([['a', 1], ['b', lookup]]);",
            "export const refs = new WeakMap();",
            "export const nothing = new Set(null);",
            "export const MAX = Math.max(1, 2, Number.MAX_SAFE_INTEGER);",
            "export const HALF = Math.PI / 2 ? Math.round(0.5) : 0;",
            "const LIMIT = 10; export const limit = Math.min(LIMIT, 5);",
            "export const count = parseInt('12', 10);",
            "export const label = String(42);",
            "function make(x) { return Boolean(x); } export const on = make({});",
            "export const list = Array.of(1, 2);",
            "export const same = Object.is(NaN, NaN);",
            // Read from the clock or a random source, and changing nothing.
            "export const at = Date.now();",
            "export const now = new Date();",
            "export const seed = Math.random();",
            "export const error = new Error('broken');",
            "export const when = new Date('2024-01-01');",
            "function clamp(x) { return Math.min(Math.max(x === null ? 0 : 1, 0), 1); } export const result = clamp(1);",
            "const make = (x) => ({ x, list: Array.isArray(x), at: new Map() }); export const result = make(1);",
            "const make = () => ({ big: Number.isFinite(1n), none: undefined }); export const result = make();",
        ] {
            assert!(init(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn to_string_and_value_of_are_taken_to_have_no_side_effects() {
        for source in [
            "export const result = String({});",
            "export const result = Math.max([1], { a: 1 });",
            "function make() { return String([1, 2]); } export const result = make();",
        ] {
            assert!(init(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn a_conversion_that_could_run_code_or_throw_stays_in_initialisation() {
        for source in [
            // An object with a method of its own is not a literal the proof reads.
            "export const result = Math.max({ valueOf() { return 1; } });",
            // Nor is one handed a function to call as its own conversion, or a
            // prototype to find one on, however deep in an array it is converted.
            "function effect() { register(); } export const result = String({ toString: effect });",
            "function effect() { register(); } export const result = Math.max({ valueOf: effect });",
            "function toString() { register(); } export const result = String({ toString });",
            "function effect() { register(); } export const result = String([[{ toString: effect }]]);",
            "export const result = String({ __proto__: null });",
            // Converting an array converts each element, and a parameter there could
            // be a symbol.
            "function make(x) { return String([x]); } export const result = make(1);",
            // A parameter could be a BigInt or a symbol, which throw when converted,
            // and a helper called from the module body throws there.
            "function make(x) { return Math.abs(x); } export const result = make(1);",
            "function make(x) { return String(x); } export const result = make(1);",
            // A function that throws on some literals, however deep in helpers the
            // call from the module body reaches it.
            "function decode(x) { return decodeURIComponent(x); } export const result = decode('%');",
            "function decode(x) { return decodeURI(x); } const outer = () => decode('%'); export const result = outer();",
            // Converting a BigInt, or a symbol, to a number throws.
            "export const result = Math.abs(1n);",
            "const BIG = 1n; export const result = Math.abs(BIG);",
            "export const result = isNaN(-1n);",
            "export const result = parseInt('12', 10n);",
            "const R = 10n; export const result = Number.parseInt('ff', R);",
            "export const result = Math.abs(Symbol.for('x'));",
            // The global symbol registry is state every module shares.
            "export const result = Symbol.for('key');",
            "export const result = new Date(1n);",
            // Entries a weak collection throws on, and entries nothing wrote out.
            "export const result = new WeakSet([1]);",
            "export const result = new WeakMap([[1, 2]]);",
            "export const result = new Map(entries);",
            "export const result = new Map(['ab']);",
            "export const result = new Set(items);",
            // Left out on purpose: each throws on some literal.
            "export const result = decodeURI('%');",
            "export const result = String.fromCodePoint(-1);",
            "export const result = new Array(-1);",
            // Methods that mutate, or read through a proxy.
            "export const result = Object.keys({});",
            "export const result = Object.assign({}, {});",
            "export const result = JSON.stringify({});",
            // A binding that only looks like the global.
            "const Math = { max: () => sideEffect() }; export const result = Math.max(1);",
            "import { Map } from './map'; export const result = new Map();",
            "export const result = Math.max(...[1, 2]);",
        ] {
            assert!(init(source).contains(&"result".to_string()), "{source}");
        }
    }

    #[test]
    fn writing_to_the_console_is_not_a_side_effect_the_app_reads() {
        for source in [
            "export const ready = console.log('ready');",
            "export const ready = console.info('ready', 1, null);",
            "function make(x) { console.log('made', x); return { x }; } export const result = make(1);",
            "const make = (x) => { console.warn(x); return x; }; export const result = make('a');",
            "function make(x) { if (x === null) { console.error({ x }); return null; } return x; } export const result = make(1);",
        ] {
            assert!(init(source).is_empty(), "{source}");
        }
        for source in [
            // A format string's `%d`, `%j` and the rest throw on a BigInt or a symbol,
            // and a first argument nobody wrote out could be one.
            "export const result = console.log('%s', {});",
            "function make(format, x) { console.log(format, x); return x; } export const result = make('a', 1);",
            // What the arguments do is still theirs.
            "export const result = console.log(register());",
            // Not the console, and not one of its methods that only writes.
            "const console = { log: () => register() }; export const result = console.log(1);",
            "export const result = console.profile('a');",
            // A statement in a helper body is still held to the subset.
            "function make(x) { register(x); return x; } export const result = make(1);",
        ] {
            assert!(init(source).contains(&"result".to_string()), "{source}");
        }
    }

    #[test]
    fn a_helper_may_read_an_import_directly() {
        for source in [
            "import { base } from './tokens'; function size(n) { return { base, n }; } export const small = size(1);",
            "import base from './tokens'; const size = (n) => [base, n]; export const small = size(1);",
            "import * as tokens from './tokens'; const all = () => tokens; export const result = all();",
            "import { base } from './tokens'; function size(n) { const b = base; return n ? b : null; } export const small = size(1);",
            "import { base } from './tokens'; const same = (n) => n === base; export const result = same(1);",
            // An import is bound before anything in the module runs.
            "function size(n) { return { base, n }; } export const small = size(1); import { base } from './tokens';",
            // Handing one to a helper reads it, as the helper's own body may.
            "import { base } from './tokens'; function size(n) { return { n }; } export const small = size(base);",
            "import { base } from './tokens'; export const shown = console.log(base);",
        ] {
            assert!(init(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn a_helper_that_does_more_with_an_import_than_read_it_stays_in_initialisation() {
        for source in [
            // A local named like the import is not the import, and reading it before
            // its own declaration throws.
            "import { base } from './tokens'; function size() { const a = base; const base = 1; return a; } export const result = size();",
            // Nor is a parameter named like it, which may be anything.
            "import { base } from './tokens'; function size(base) { return base(); } export const result = size(1);",
            // A member read through an import may run a getter, however it is imported.
            "import { theme } from './theme'; function color() { return theme.colors.primary; } export const result = color();",
            "import * as tokens from './tokens'; const size = () => tokens.base; export const result = size();",
            "import tokens from './tokens'; function size(n) { return { n, base: tokens.base }; } export const result = size(1);",
            // Calling or constructing an import is not reading it.
            "import { format } from './format'; function label(n) { return format(n); } export const result = label(1);",
            "import * as text from './format'; const label = (n) => text.format(n); export const result = label(1);",
            "import { Size } from './size'; function make() { return new Size(); } export const result = make();",
            // Writing an import throws.
            "import { base } from './tokens'; function size() { base = 1; return 1; } export const result = size();",
            "import { base } from './tokens'; function size() { base += 1; return 1; } export const result = size();",
            "import { base } from './tokens'; function size() { base++; return 1; } export const result = size();",
            // Reading an import clears nothing else the body does.
            "import { base } from './tokens'; function size() { register(); return base; } export const result = size();",
            "import { base } from './tokens'; const size = () => ({ base, id: register() }); export const result = size();",
            // An imported value may be a BigInt or a symbol, which converting throws on,
            // in a helper or in the module body.
            "import { base } from './tokens'; function size() { return String(base); } export const result = size();",
            "import { base } from './tokens'; const size = (n) => Math.max(base, 1); export const result = size(1);",
            "import { base } from './tokens'; export const result = Math.abs(base);",
            "import { base } from './tokens'; function size(n) { return base * n; } export const result = size(1);",
            "import { base } from './tokens'; function size() { return `${base}px`; } export const result = size();",
        ] {
            assert!(init(source).contains(&"result".to_string()), "{source}");
        }
    }

    #[test]
    fn creating_a_function_runs_nothing() {
        for source in [
            // An object of actions, as a store's creator returns. No body runs until
            // it is called, whatever it holds.
            "function make() { return { count: 0, inc: () => 1 }; } export const store = make();",
            "function make() { return { count: 0, inc() { return register(); } }; } export const store = make();",
            "const make = () => ({ get count() { return register(); }, set count(v) { register(v); } }); export const store = make();",
            "const make = (set) => ({ count: 0, inc: () => set((s) => ({ count: s.count + 1 })) }); export const store = make(null);",
            // A function returned whole.
            "function make() { return () => register(); } export const run = make();",
            "function make() { return function () { return register(); }; } export const run = make();",
            "const make = () => { const run = () => register(); return { run }; }; export const store = make();",
            // An async or generator function is created as any other, and nothing in
            // it runs until it is called.
            "const make = () => ({ load: async () => fetch('/items') }); export const store = make();",
            "function make() { return async function () { await register(); }; } export const load = make();",
            "function make() { return function* () { yield register(); }; } export const items = make();",
            // A default runs when the function is called, not when it is created.
            "function make() { return (x = register()) => x; } export const run = make();",
            // A read in the body happens when it is called, so what it reads need not
            // exist yet.
            "function make() { return () => LATER; } export const read = make(); const LATER = 1;",
            // Handing a function to a proven helper creates it and no more.
            "function make(x) { return 1; } export const result = make(() => register());",
            "function make(x) { return 1; } export const result = make({ get x() { return 1; } });",
        ] {
            assert!(init(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn what_runs_when_a_function_is_created_or_called_stays_in_initialisation() {
        for source in [
            // A computed key runs where the object is created, method or not.
            "function make() { return { [key()]() { return 1; } }; } export const result = make();",
            "function make() { return { get [key()]() { return 1; } }; } export const result = make();",
            // Defining a class runs its heritage and static members, so a class
            // expression is not taken for a plain value.
            "function make() { return class { static { register(); } }; } export const result = make();",
            "function make() { return class { static x = register(); }; } export const result = make();",
            "function make() { return class extends Base() {}; } export const result = make();",
            // Calling a function runs its body, however it was made.
            "export const result = (() => register())();",
            "function make() { return (() => register())(); } export const result = make();",
            "function make() { const run = () => register(); return run(); } export const result = make();",
            "function make() { return () => register(); } export const result = make()();",
            "function make() { return () => register(); } const run = make(); export const result = run();",
            "function make() { return { inc: () => register() }; } export const result = make().inc();",
            "function make() { return () => register(); } const run = make(); function use() { return run(); } export const result = use();",
            "function apply(run) { return run(); } export const result = apply(() => register());",
            "const make = () => ({ load: async () => 1 }); export const result = make().load();",
            // A function held by a literal that is converted is called by the
            // conversion.
            "export const result = Math.max({ valueOf: () => register() });",
            "export const result = String([{ toString() { return register(); } }]);",
            "function make() { return String({ valueOf: () => register(), toString() { return 'a'; } }); } export const result = make();",
        ] {
            assert!(init(source).contains(&"result".to_string()), "{source}");
        }
    }

    #[test]
    fn a_function_created_early_is_still_judged_where_it_is_called() {
        // Creating a function that reads a `const` declared further down runs
        // nothing. Calling it before that declaration has run reads the `const`
        // early and throws, and that call is judged by the rules for any call, which
        // do not prove a call to a value a helper returned.
        let source = "function make() { return () => LATER; } \
                      const read = make(); \
                      export const result = read(); \
                      const LATER = 1;";
        // Initialisation then reaches what the call reads, down to the `const` it
        // reads too early, so an edit to any of them reaches every importer.
        let found = init(source);
        for name in ["result", "read", "make", "LATER"] {
            assert!(found.contains(&name.to_string()), "{name}: {source}");
        }
        for source in [
            "function make() { return () => LATER; } export const result = make()(); const LATER = 1;",
            "const make = () => ({ get: () => LATER }); export const result = make().get(); const LATER = 1;",
            "function make() { return () => inner(); } export const result = make()(); const inner = () => 1;",
            "function make() { return { read() { return LATER; } }; } const store = make(); export const result = store.read(); const LATER = 1;",
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
            "{ get [key()]() { return 1; } }",
            "class { static { register(); } }",
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
