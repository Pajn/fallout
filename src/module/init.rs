//! What runs when the module is evaluated.
//!
//! `ModuleInit(f)` is the node every importer of `f` depends on. It collects the
//! declarations whose values are computed at import time, either because a top-level
//! statement uses them or because their own initialiser may have side effects.

use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_span::GetSpan;

use super::decls::DeclDraft;
use super::parse::{Ctx, span_of};
use super::{Decl, DeclId};

/// Declarations module initialisation depends on.
pub(crate) fn collect(
    ctx: &Ctx<'_>,
    program: &Program<'_>,
    drafts: &[DeclDraft],
    decls: &[Decl],
) -> Vec<DeclId> {
    let mut init: Vec<DeclId> = Vec::new();

    for (index, statement) in program.body.iter().enumerate() {
        let declares = drafts.iter().any(|draft| draft.statement == index);

        if !declares {
            // A top-level statement that declares nothing runs for its effect. Every
            // declaration it names is part of initialisation.
            for decl in referenced_decls(ctx, statement, drafts) {
                push_unique(&mut init, decl);
            }
            continue;
        }

        // An initialiser that may have side effects runs at import time whether or
        // not anyone reads the binding.
        if statement_has_impure_initialiser(statement) {
            for (id, draft) in drafts.iter().enumerate() {
                if draft.statement == index {
                    push_unique(&mut init, id as DeclId);
                }
            }
        }
    }

    // Initialisation reaches whatever those declarations reach.
    let mut queue: Vec<DeclId> = init.clone();
    while let Some(current) = queue.pop() {
        for &next in &decls[current as usize].refs {
            if !init.contains(&next) {
                init.push(next);
                queue.push(next);
            }
        }
    }

    init.sort_unstable();
    init
}

/// Top-level declarations named anywhere inside `statement`.
fn referenced_decls(ctx: &Ctx<'_>, statement: &Statement<'_>, drafts: &[DeclDraft]) -> Vec<DeclId> {
    let scoping = ctx.semantic.scoping();
    let root = scoping.root_scope_id();
    let span = span_of(statement.span());

    let mut found = Vec::new();
    for symbol_id in scoping.symbol_ids() {
        if scoping.symbol_scope_id(symbol_id) != root {
            continue;
        }
        let name = scoping.symbol_name(symbol_id);
        let Some(decl) = drafts.iter().position(|d| d.name == name) else {
            continue;
        };

        for reference_id in scoping.get_resolved_reference_ids(symbol_id) {
            let node_id = scoping.get_reference(*reference_id).node_id();
            let at = span_of(ctx.semantic.nodes().get_node(node_id).kind().span());
            if span.contains(at.start) {
                push_unique(&mut found, decl as DeclId);
                break;
            }
        }
    }
    found
}

/// The conservative rule for this step: a call, a construction, an `await`, a tagged
/// template, or an assignment to a member is impure. Later steps narrow this.
fn statement_has_impure_initialiser(statement: &Statement<'_>) -> bool {
    let declaration = match statement {
        Statement::ExportDeclaration(export) => Some(&export.declaration),
        Statement::ExportDefaultDeclaration(export) => {
            return expression_is_impure(&export.declaration);
        }
        statement => statement.as_declaration(),
    };

    let Some(Declaration::VariableDeclaration(variable)) = declaration else {
        // A function or class declaration binds without running anything.
        return false;
    };

    variable
        .declarations
        .iter()
        .filter_map(|declarator| declarator.init.as_ref())
        .any(expression_is_impure)
}

fn expression_is_impure<'a, E: ImpurityCheck<'a>>(expression: &E) -> bool {
    expression.check_impurity()
}

pub(crate) trait ImpurityCheck<'a> {
    fn check_impurity(&self) -> bool;
}

impl<'a> ImpurityCheck<'a> for Expression<'a> {
    fn check_impurity(&self) -> bool {
        let mut detector = ImpureDetector { impure: false };
        detector.visit_expression(self);
        detector.impure
    }
}

impl<'a> ImpurityCheck<'a> for ExportDefaultDeclarationKind<'a> {
    fn check_impurity(&self) -> bool {
        match self {
            // `export default function f() {}` binds without running anything.
            ExportDefaultDeclarationKind::FunctionDeclaration(_)
            | ExportDefaultDeclarationKind::ClassDeclaration(_)
            | ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => false,
            expression => {
                let mut detector = ImpureDetector { impure: false };
                if let Some(expression) = expression.as_expression() {
                    detector.visit_expression(expression);
                }
                detector.impure
            }
        }
    }
}

struct ImpureDetector {
    impure: bool,
}

impl<'a> Visit<'a> for ImpureDetector {
    fn visit_call_expression(&mut self, expr: &CallExpression<'a>) {
        self.impure = true;
        walk::walk_call_expression(self, expr);
    }

    fn visit_new_expression(&mut self, expr: &NewExpression<'a>) {
        self.impure = true;
        walk::walk_new_expression(self, expr);
    }

    fn visit_await_expression(&mut self, expr: &AwaitExpression<'a>) {
        self.impure = true;
        walk::walk_await_expression(self, expr);
    }

    fn visit_tagged_template_expression(&mut self, expr: &TaggedTemplateExpression<'a>) {
        self.impure = true;
        walk::walk_tagged_template_expression(self, expr);
    }

    fn visit_assignment_expression(&mut self, expr: &AssignmentExpression<'a>) {
        if !matches!(expr.left, AssignmentTarget::AssignmentTargetIdentifier(_)) {
            self.impure = true;
        }
        walk::walk_assignment_expression(self, expr);
    }

    // A function body does not run until it is called, so what it contains says
    // nothing about the initialiser's purity.
    fn visit_function(&mut self, _function: &Function<'a>, _flags: oxc_semantic::ScopeFlags) {}

    fn visit_arrow_function_expression(&mut self, _expr: &ArrowFunctionExpression<'a>) {}
}

fn push_unique(list: &mut Vec<DeclId>, value: DeclId) {
    if !list.contains(&value) {
        list.push(value);
    }
}
