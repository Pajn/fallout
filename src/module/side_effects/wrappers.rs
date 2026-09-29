//! Zustand middleware around a store's creator.
//!
//! `create(immer((set) => …))` hands `create` the creator `immer` returns, not the
//! one written out, so the call `create` is handed is itself a call. Evaluating it
//! only builds that creator, and calling the creator calls the one it wraps, so a
//! middleware the table lists is looked through: its call runs nothing but its
//! arguments, and calling what it returns is quiet where calling what it wraps is.
//! Any other middleware, or any other call, is not, wherever it sits in the chain.
//!
//! A middleware is known by the import its callee resolves to, as an entry of the
//! pure list is. Whether that import lands in a file of the app, where the name is
//! Zustand's and the function is not, is the graph's to say, so each one a proof
//! relies on is handed back with it. See [`crate::factories::rules::WRAPPERS`].

use oxc_ast::ast::*;
use oxc_span::GetSpan;

use super::SideEffects;
use crate::factories::rules::{Wrapper, Wraps};
use crate::module::parse::span_of;
use crate::module::{SourceId, Span};

/// A middleware a proof that calling a creator runs nothing relies on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::module) struct Wrapped {
    /// The import it was matched by.
    pub source: SourceId,
    /// Where its callee is written, which runs as the store is made, as the
    /// factory does.
    pub callee: Span,
}

impl SideEffects<'_, '_> {
    /// Whether evaluating `argument`, an argument of a call that may be a factory's,
    /// where it is written, runs anything. A middleware's call only builds the
    /// creator it returns, so it runs what its own arguments run and nothing else.
    pub(in crate::module) fn evaluating_runs(&self, argument: &Expression<'_>) -> bool {
        match self.wrapper_call(argument) {
            Some((_, _, call)) => call.arguments.iter().any(|argument| {
                argument
                    .as_expression()
                    .is_none_or(|argument| self.evaluating_runs(argument))
            }),
            None => self.runs(argument),
        }
    }

    /// The middleware `function` is wrapped in, where calling it once at `at`, with
    /// arguments nobody here knows anything about, is proven to run nothing and not
    /// to throw. `None` where it is not.
    ///
    /// A function written out in place is proven by the local-helper proof. A
    /// middleware's call is proven where calling what it wraps is, with no argument
    /// the middleware does not take. `combine` also merges its initial state and
    /// what the creator returns with `Object.assign`, which reads every property of
    /// each, so both must be object literals written out whose properties hold
    /// values rather than getters, and the initial state must run nothing.
    pub(in crate::module) fn called_quietly(
        &self,
        function: &Expression<'_>,
        at: u32,
    ) -> Option<Vec<Wrapped>> {
        let mut wrapped = Vec::new();
        self.unwrapped_quietly(function, at, &mut wrapped)
            .then_some(wrapped)
    }

    fn unwrapped_quietly(
        &self,
        function: &Expression<'_>,
        at: u32,
        wrapped: &mut Vec<Wrapped>,
    ) -> bool {
        let Some((wrapper, source, call)) = self.wrapper_call(function) else {
            return self.local.invoked(function, at);
        };
        wrapped.push(Wrapped {
            source,
            callee: span_of(call.callee.span()),
        });
        let Some(arguments) = call
            .arguments
            .iter()
            .map(Argument::as_expression)
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        match (wrapper.wraps, arguments.as_slice()) {
            (Wraps::Creator, [creator]) => self.unwrapped_quietly(creator, at, wrapped),
            (Wraps::Merged, [initial, creator]) => {
                holds_values(initial)
                    && !self.runs(initial)
                    && self.returns_values(creator)
                    && self.unwrapped_quietly(creator, at, wrapped)
            }
            _ => false,
        }
    }

    /// Whether what calling `function` returns is an object whose own properties
    /// are all values, so that reading each of them runs nothing.
    ///
    /// A function written out must return an object literal that holds values, as
    /// the last statement of its body and the only return in it. A middleware that
    /// returns what its creator does returns that, and `combine` returns the object
    /// `Object.assign` made, whose properties are values whatever it copied.
    fn returns_values(&self, function: &Expression<'_>) -> bool {
        if let Some((wrapper, _, call)) = self.wrapper_call(function) {
            return match wrapper.wraps {
                Wraps::Creator => call
                    .arguments
                    .first()
                    .and_then(Argument::as_expression)
                    .is_some_and(|creator| self.returns_values(creator)),
                Wraps::Merged => true,
            };
        }
        let statements = match function.get_inner_expression() {
            Expression::ArrowFunctionExpression(arrow) => match &arrow.body {
                ArrowFunctionBody::FunctionBody(body) => &body.statements,
                body => return body.as_expression().is_some_and(holds_values),
            },
            Expression::FunctionExpression(function) => match &function.body {
                Some(body) => &body.statements,
                None => return false,
            },
            _ => return false,
        };
        // Every statement the proof reads that could return is kept out, so the
        // last is the only return.
        let [rest @ .., Statement::ReturnStatement(returned)] = statements.as_slice() else {
            return false;
        };
        rest.iter().all(|statement| {
            matches!(
                statement,
                Statement::VariableDeclaration(_)
                    | Statement::ExpressionStatement(_)
                    | Statement::EmptyStatement(_)
            )
        }) && returned.argument.as_ref().is_some_and(holds_values)
    }

    /// The middleware `expression` calls, the import it was matched by, and the
    /// call.
    fn wrapper_call<'e, 'b>(
        &self,
        expression: &'e Expression<'b>,
    ) -> Option<(&'static Wrapper, SourceId, &'e CallExpression<'b>)> {
        let Expression::CallExpression(call) = expression.get_inner_expression() else {
            return None;
        };
        let (wrapper, source) = self.local.imports().wrapper(&call.callee)?;
        Some((wrapper, source, call))
    }
}

/// An object literal whose properties are each written out as a key and a value,
/// or a method, so that reading any of them runs nothing: no getter, no spread,
/// and no computed key.
fn holds_values(expression: &Expression<'_>) -> bool {
    let Expression::ObjectExpression(object) = expression.get_inner_expression() else {
        return false;
    };
    object.properties.iter().all(|property| match property {
        ObjectPropertyKind::ObjectProperty(property) => {
            property.kind == PropertyKind::Init && !property.computed
        }
        ObjectPropertyKind::SpreadProperty(_) => false,
    })
}
