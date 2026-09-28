//! What runs when the module is evaluated.
//!
//! `ModuleInit(f)` is the node every importer of `f` depends on. It collects the
//! declarations whose values are computed at import time, either because a top-level
//! statement uses them or because their own initialiser may have side effects.

use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_span::GetSpan;

use ahash::{AHashMap, AHashSet};

use super::cjs;
use super::decls::{DeclDraft, ImportBinding};
use super::local_pure::LocalPure;
use super::parse::{Ctx, span_of};
use super::{Decl, DeclId, ImportTarget};
use crate::pure::PureList;

/// Where a name in this file comes from, for deciding whether a call on it is one
/// the project has declared pure.
pub(crate) type Origins<'a> = AHashMap<&'a str, (&'a str, &'a str)>;

/// Each imported binding as `local name -> (module specifier, exported name)`, with
/// `default` and `*` standing for the two unnamed import forms.
pub(crate) fn origins<'a>(imports: &'a [ImportBinding], sources: &'a [String]) -> Origins<'a> {
    let mut map = Origins::default();
    for binding in imports {
        let Some(source) = sources.get(binding.reference.source as usize) else {
            continue;
        };
        let exported = match &binding.reference.target {
            ImportTarget::Named(name) | ImportTarget::Member { export: name, .. } => name.as_str(),
            ImportTarget::Namespace => "*",
        };
        map.insert(binding.local.as_str(), (source.as_str(), exported));
    }
    map
}

/// Declarations module initialisation depends on.
pub(crate) fn collect(
    ctx: &Ctx<'_>,
    program: &Program<'_>,
    drafts: &[DeclDraft],
    decls: &[Decl],
    origins: &Origins<'_>,
    pure: &PureList,
    conditional: &AHashSet<usize>,
) -> (Vec<DeclId>, Vec<DeclId>) {
    let local = LocalPure::infer(ctx, program);
    let mut init: Vec<DeclId> = Vec::new();
    let mut maybe: Vec<DeclId> = Vec::new();

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
        // not anyone reads the binding. One this module can clear is not asked about
        // again below.
        if !statement_has_impure_initialiser(statement, origins, pure, &local) {
            continue;
        }

        // A call that may be a factory's runs nothing else if its arguments do not:
        // whether the call itself does is the graph's to say, once it knows the
        // callee. See [`super::factories`].
        if conditional.contains(&index)
            && let Some((_, call)) = super::factories::call_of(statement)
            && !call.arguments.iter().any(|argument| {
                argument
                    .as_expression()
                    .is_none_or(|argument| argument.check_impurity(origins, pure, &local))
            })
        {
            for (id, draft) in drafts.iter().enumerate() {
                if draft.statement == index {
                    push_unique(&mut maybe, id as DeclId);
                }
            }
            continue;
        }

        for (id, draft) in drafts.iter().enumerate() {
            if draft.statement == index {
                push_unique(&mut init, id as DeclId);
            }
        }
    }

    // Initialisation reaches whatever those declarations reach, including an object
    // one of them reads a property of.
    let mut queue: Vec<DeclId> = init.clone();
    while let Some(current) = queue.pop() {
        let entry = &decls[current as usize];
        let objects = entry.member_refs.iter().map(|(object, _)| object);
        for &next in entry.refs.iter().chain(objects) {
            if !init.contains(&next) {
                init.push(next);
                queue.push(next);
            }
        }
    }

    // A conditional declaration an effectful one reads is part of initialisation
    // either way, so it may be in both lists.
    init.sort_unstable();
    (init, maybe)
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
fn statement_has_impure_initialiser(
    statement: &Statement<'_>,
    origins: &Origins<'_>,
    pure: &PureList,
    local: &LocalPure<'_, '_>,
) -> bool {
    let declaration = match statement {
        Statement::ExportDeclaration(export) => Some(&export.declaration),
        Statement::ExportDefaultDeclaration(export) => {
            return export.declaration.check_impurity(origins, pure, local);
        }
        // Only a statement that declares something reaches here, so an expression
        // statement is a CommonJS export: either the assignment that fills the table,
        // whose initialiser is the value it assigns, or a defined property, which
        // stores what it is handed without running any of it.
        Statement::ExpressionStatement(statement) => {
            return cjs::assigned_value(&statement.expression)
                .is_some_and(|value| value.check_impurity(origins, pure, local));
        }
        statement => statement.as_declaration(),
    };

    let variable = match declaration {
        Some(Declaration::VariableDeclaration(variable)) => variable,
        Some(Declaration::ClassDeclaration(class)) => {
            return class.check_impurity(origins, pure, local);
        }
        // Each member's value is computed when the enum is, and a `const enum` is
        // read the same way because type erasure keeps it.
        Some(Declaration::TSEnumDeclaration(enumeration)) => {
            return enumeration
                .body
                .members
                .iter()
                .filter_map(|member| member.initializer.as_ref())
                .any(|init| init.check_impurity(origins, pure, local));
        }
        // A function declaration binds without running anything.
        _ => return false,
    };

    variable
        .declarations
        .iter()
        .filter_map(|declarator| declarator.init.as_ref())
        .any(|init| {
            // `var _default = (exports.default = …)`, a compiler's `export default`.
            // Filling the table is the export, not an effect on anybody else, so what
            // decides whether this runs anything is the value alone.
            cjs::assigned_value(init)
                .unwrap_or(init)
                .check_impurity(origins, pure, local)
        })
}

trait ImpurityCheck<'a> {
    fn check_impurity(
        &self,
        origins: &Origins<'_>,
        pure: &PureList,
        local: &LocalPure<'_, '_>,
    ) -> bool;
}

impl<'a> ImpurityCheck<'a> for Expression<'a> {
    fn check_impurity(
        &self,
        origins: &Origins<'_>,
        pure: &PureList,
        local: &LocalPure<'_, '_>,
    ) -> bool {
        let mut detector = ImpureDetector::new(origins, pure, local);
        detector.visit_expression(self);
        detector.impure
    }
}

impl<'a> ImpurityCheck<'a> for Class<'a> {
    fn check_impurity(
        &self,
        origins: &Origins<'_>,
        pure: &PureList,
        local: &LocalPure<'_, '_>,
    ) -> bool {
        let mut detector = ImpureDetector::new(origins, pure, local);
        detector.visit_class(self);
        detector.impure
    }
}

impl<'a> ImpurityCheck<'a> for ExportDefaultDeclarationKind<'a> {
    fn check_impurity(
        &self,
        origins: &Origins<'_>,
        pure: &PureList,
        local: &LocalPure<'_, '_>,
    ) -> bool {
        match self {
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                class.check_impurity(origins, pure, local)
            }
            // `export default function f() {}` binds without running anything.
            ExportDefaultDeclarationKind::FunctionDeclaration(_)
            | ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => false,
            expression => {
                let mut detector = ImpureDetector::new(origins, pure, local);
                if let Some(expression) = expression.as_expression() {
                    detector.visit_expression(expression);
                }
                detector.impure
            }
        }
    }
}

struct ImpureDetector<'c, 'o, 'a> {
    local: &'c LocalPure<'c, 'a>,
    impure: bool,
    origins: &'c Origins<'o>,
    pure: &'c PureList,
}

impl<'c, 'o, 'a> ImpureDetector<'c, 'o, 'a> {
    fn new(origins: &'c Origins<'o>, pure: &'c PureList, local: &'c LocalPure<'c, 'a>) -> Self {
        Self {
            impure: false,
            local,
            origins,
            pure,
        }
    }

    /// Is this callee free of side effects?
    ///
    /// `annotated` is the `/* @__PURE__ */` comment, which is the author of the call
    /// site speaking. The list is the project speaking about someone else's
    /// function, and is honoured only where the callee is reached from the import
    /// the entry names, so a local binding of the same name is not covered.
    fn callee_is_pure(&self, annotated: bool, callee: &Expression<'_>) -> bool {
        if annotated {
            return true;
        }
        let Some((root, members)) = callee_path(callee) else {
            return false;
        };
        let Some((source, exported)) = self.origins.get(root) else {
            return false;
        };

        let mut path = Vec::with_capacity(members.len() + 1);
        path.push(*exported);
        path.extend(members);
        self.pure.contains(source, &path)
    }
}

impl<'a, 'c, 'o> Visit<'a> for ImpureDetector<'c, 'o, '_> {
    fn visit_call_expression(&mut self, expr: &CallExpression<'a>) {
        if !self.callee_is_pure(expr.pure, &expr.callee)
            && !self.local.call(expr)
            && !self.local.freezes(expr)
        {
            self.impure = true;
        }
        // A pure callee says nothing about its arguments, which still run.
        walk::walk_call_expression(self, expr);
    }

    fn visit_new_expression(&mut self, expr: &NewExpression<'a>) {
        if !self.callee_is_pure(expr.pure, &expr.callee) && !self.local.constructs(expr) {
            self.impure = true;
        }
        walk::walk_new_expression(self, expr);
    }

    fn visit_await_expression(&mut self, expr: &AwaitExpression<'a>) {
        self.impure = true;
        walk::walk_await_expression(self, expr);
    }

    fn visit_tagged_template_expression(&mut self, expr: &TaggedTemplateExpression<'a>) {
        // A tagged template has no annotation field to read, so only the list can
        // clear it.
        if !self.callee_is_pure(false, &expr.tag) {
            self.impure = true;
        }
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

    // Defining a class evaluates its heritage, every computed key, and its static
    // fields and blocks in order, so those run wherever the class is written. An
    // instance field waits for a construction and a method for a call, as a
    // function body does.
    fn visit_class(&mut self, class: &Class<'a>) {
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

    // A static block is a body of statements, which can do more than an expression
    // can: throw, loop over an iterator, branch. So only a declaration or an
    // expression statement is judged by what it contains, and any other statement
    // counts as running something.
    fn visit_static_block(&mut self, block: &StaticBlock<'a>) {
        for statement in &block.body {
            let judged = match statement {
                Statement::VariableDeclaration(variable) => matches!(
                    variable.kind,
                    VariableDeclarationKind::Var
                        | VariableDeclarationKind::Let
                        | VariableDeclarationKind::Const
                ),
                Statement::ExpressionStatement(_)
                | Statement::FunctionDeclaration(_)
                | Statement::ClassDeclaration(_)
                | Statement::EmptyStatement(_) => true,
                _ => false,
            };
            if !judged {
                self.impure = true;
            }
            self.visit_statement(statement);
        }
    }
}

/// Splits `A.b.c` into its root name and the members read from it. `None` when the
/// callee is anything else, such as a call on a call or a computed member.
fn callee_path<'a>(callee: &'a Expression<'a>) -> Option<(&'a str, Vec<&'a str>)> {
    match callee {
        Expression::Identifier(ident) => Some((ident.name.as_str(), Vec::new())),
        Expression::StaticMemberExpression(member) => {
            let (root, mut path) = callee_path(&member.object)?;
            path.push(member.property.name.as_str());
            Some((root, path))
        }
        _ => None,
    }
}

fn push_unique(list: &mut Vec<DeclId>, value: DeclId) {
    if !list.contains(&value) {
        list.push(value);
    }
}

#[cfg(test)]
mod tests {
    use crate::module::{FineModule, ModuleAnalysis, Reading, parse::analyse_source};
    use std::path::Path;

    fn module(source: &str) -> Box<FineModule> {
        let (ModuleAnalysis::Fine(module), _) =
            analyse_source(Path::new("lib.ts"), source, &Reading::default()).unwrap()
        else {
            panic!("expected fine module: {source}")
        };
        module
    }

    /// The declarations module initialisation reaches, by name.
    fn init(source: &str) -> Vec<String> {
        let module = module(source);
        module
            .init_decls
            .iter()
            .map(|&id| module.decls[id as usize].name.clone())
            .collect()
    }

    #[test]
    fn what_runs_when_a_class_is_defined_is_initialisation() {
        for (source, name) in [
            ("export class Widget { static id = register(1); }", "Widget"),
            ("class Widget { static { register(1); } }", "Widget"),
            ("export class Widget extends mixin(1) {}", "Widget"),
            ("export class Widget { [key()]() {} }", "Widget"),
            // The key of an instance field is computed with the class, even though
            // its value waits for a construction.
            ("export class Widget { [key()] = 1; }", "Widget"),
            (
                "export class Widget { static accessor id = register(1); }",
                "Widget",
            ),
            (
                "export default class { static id = register(1); }",
                "default",
            ),
            (
                "export default class Widget { static { register(1); } }",
                "default",
            ),
            (
                "export const Widget = class { static id = register(1); };",
                "Widget",
            ),
            ("const Widget = class extends mixin(1) {};", "Widget"),
            // A class declared inside a static block is defined when the block runs.
            (
                "export class Widget { static { class Inner { static id = register(1); } } }",
                "Widget",
            ),
        ] {
            assert!(init(source).contains(&name.to_string()), "{source}");
        }
    }

    #[test]
    fn a_static_block_that_may_stop_the_module_loading_is_initialisation() {
        for source in [
            // `new Error("…")` is pure, but throwing it stops the module loading.
            "export class Widget { static { throw new Error('no'); } }",
            // Iterating runs whatever the iterator does.
            "const list = [1]; export class Widget { static { for (const item of list) {} } }",
            "export class Widget { static { if (flag) { register(1); } } }",
        ] {
            assert!(init(source).contains(&"Widget".to_string()), "{source}");
        }
    }

    #[test]
    fn what_waits_for_a_construction_or_a_call_is_not_initialisation() {
        for source in [
            "export class Widget { id = register(1); }",
            "export class Widget { accessor id = register(1); }",
            "export class Widget { run() { register(1); } }",
            "export class Widget { static run() { register(1); } }",
            "export class Widget { static make = () => register(1); }",
            "export class Widget { constructor() { register(1); } }",
            "export const Widget = class { id = register(1); };",
            "export default class { run() { register(1); } }",
            "import { Base } from './base'; export class Widget extends Base {}",
            "export class Widget { static id = 1; static { const local = [Widget.id]; } }",
            "export class Widget { ['literal'] = register(1); }",
        ] {
            assert!(init(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn a_static_member_built_by_a_pure_call_is_not_initialisation() {
        for source in [
            "import { memo } from 'react'; export class Widget { static View = memo(1); }",
            "import * as React from 'react'; export class Widget { static View = React.memo(1); }",
            "import { memo } from 'react'; export class Widget { static { memo(1); } }",
            "function make(x) { return { x }; } export class Widget { static id = make(1); }",
            "export class Widget { static id = /* @__PURE__ */ register(1); }",
            "import { memo } from 'react'; export const Widget = class { static View = memo(1); };",
        ] {
            assert!(init(source).is_empty(), "{source}");
        }
    }

    #[test]
    fn an_enum_member_computed_by_a_call_is_initialisation() {
        for source in [
            "export enum Size { Small = register(1) }",
            "enum Size { Small = 1, Large = register(2) }",
            // Type erasure keeps a `const enum`, as a compiler reading one file at a
            // time does, so it is read like any other.
            "export const enum Size { Small = register(1) }",
        ] {
            assert!(init(source).contains(&"Size".to_string()), "{source}");
        }
        for source in [
            "export enum Size { Small, Large }",
            "export enum Size { Small = 1, Large = Small << 1, Label = 'l' }",
            "import { memo } from 'react'; export enum Size { Small = memo(1) }",
        ] {
            assert!(init(source).is_empty(), "{source}");
        }
    }
}
