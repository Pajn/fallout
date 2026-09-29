//! Which imports code run at load reads.
//!
//! Reading an imported binding runs nothing of this file's. But where a project
//! defers each import to the first use of its binding, reading one is what evaluates
//! the module it names, there and then. Whether the project does is the anchor's to
//! say, not this file's, so this only says whether an import is read, and the graph
//! decides what that means.
//!
//! What runs at load is what [`super::Detector`] walks: a function body waits for a
//! call, and a class runs its heritage, its computed keys and its static members as
//! it is defined. A call to a function declared at the top of this file runs its
//! body where the call is written, so that body is read too, as far as the calls in
//! it go, whether or not the local-helper proof clears it: one it does not makes the
//! declaration initialisation for what it runs anyway. A call to anything else is
//! not followed. Unless the pure list or an annotation clears it, the declaration is
//! initialisation for what it runs, whatever it reads, and where one of those does,
//! it speaks for what the callee does.

use oxc_ast::AstKind;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_semantic::{AstNodes, Scoping, SymbolId};

pub(super) struct Reader<'s, 'a> {
    scoping: &'s Scoping,
    nodes: &'s AstNodes<'a>,
    pub(super) reads: bool,
    /// The functions of this file whose bodies have been read, so that one called
    /// twice, or calling itself, is read once.
    entered: Vec<SymbolId>,
}

impl<'s, 'a> Reader<'s, 'a> {
    pub(super) fn new(scoping: &'s Scoping, nodes: &'s AstNodes<'a>) -> Self {
        Self {
            scoping,
            nodes,
            reads: false,
            entered: Vec::new(),
        }
    }

    /// The top-level binding `identifier` names, if it names one.
    fn top_level(&self, identifier: &IdentifierReference<'_>) -> Option<SymbolId> {
        let reference = self.scoping.get_reference(identifier.reference_id.get()?);
        let symbol = reference.symbol_id()?;
        (self.scoping.symbol_scope_id(symbol) == self.scoping.root_scope_id()).then_some(symbol)
    }

    /// Does `identifier` read the value of an import? A type names nothing that
    /// runs, and a type-only import is gone before anything runs.
    fn reads_import(&self, identifier: &IdentifierReference<'_>) -> bool {
        let Some(reference) = identifier
            .reference_id
            .get()
            .map(|id| self.scoping.get_reference(id))
        else {
            return false;
        };
        let Some(symbol) = reference.symbol_id() else {
            return false;
        };
        let flags = self.scoping.symbol_flags(symbol);
        reference.is_read() && flags.is_import() && !flags.is_type_import()
    }

    /// Reads the body of the function of this file that `callee` names, which a
    /// call runs where it is written.
    fn enter(&mut self, callee: &IdentifierReference<'_>) {
        let Some(symbol) = self.top_level(callee) else {
            return;
        };
        if self.entered.contains(&symbol) {
            return;
        }
        self.entered.push(symbol);
        match self
            .nodes
            .get_node(self.scoping.symbol_declaration(symbol))
            .kind()
        {
            AstKind::Function(function) => self.visit_body(function),
            AstKind::VariableDeclarator(declarator) => {
                match declarator
                    .init
                    .as_ref()
                    .map(Expression::get_inner_expression)
                {
                    Some(Expression::ArrowFunctionExpression(arrow)) => {
                        self.visit_arrow_function_body(&arrow.body);
                    }
                    Some(Expression::FunctionExpression(function)) => self.visit_body(function),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn visit_body(&mut self, function: &Function<'a>) {
        if let Some(body) = &function.body {
            self.visit_function_body(body);
        }
    }
}

// What a function of this file holds lives as long as the file's syntax tree, which
// outlives any part of it being read.
impl<'x, 'a: 'x> Visit<'x> for Reader<'_, 'a> {
    fn visit_identifier_reference(&mut self, identifier: &IdentifierReference<'x>) {
        if self.reads_import(identifier) {
            self.reads = true;
        }
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'x>) {
        if let Expression::Identifier(callee) = &call.callee {
            self.enter(callee);
        }
        walk::walk_call_expression(self, call);
    }

    // A function body does not run until it is called, so what it reads is read then.
    fn visit_function(&mut self, _function: &Function<'x>, _flags: oxc_semantic::ScopeFlags) {}

    fn visit_arrow_function_expression(&mut self, _expr: &ArrowFunctionExpression<'x>) {}

    // Defining a class evaluates its heritage, every computed key, and its static
    // fields and blocks. An instance field waits for a construction and a method for
    // a call.
    fn visit_class(&mut self, class: &Class<'x>) {
        if let Some(heritage) = &class.heritage {
            self.visit_expression(&heritage.expression);
        }
        for element in &class.body.body {
            let (key, computed, value, is_static) = match element {
                ClassElement::StaticBlock(block) => {
                    self.visit_static_block(block);
                    continue;
                }
                ClassElement::MethodDefinition(method) => {
                    (&method.key, method.computed, None, false)
                }
                ClassElement::PropertyDefinition(property) => (
                    &property.key,
                    property.computed,
                    property.value.as_ref(),
                    property.r#static,
                ),
                ClassElement::AccessorProperty(property) => (
                    &property.key,
                    property.computed,
                    property.value.as_ref(),
                    property.r#static,
                ),
                ClassElement::TSIndexSignature(_) => continue,
            };
            if computed {
                self.visit_property_key(key);
            }
            if is_static && let Some(value) = value {
                self.visit_expression(value);
            }
        }
    }
}
